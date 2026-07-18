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
        max_output_lines = 3000,
    },
    provider = {
        default_model = "anthropic/claude-sonnet-4-6",
    },
    storage = {
        max_log_files = 5,
    },
    plugins = {
        bash = { timeout_secs = 180 },
        index = { max_file_size_mb = 4 },
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
| `always_fast` | bool | `false` | Start every session with Anthropic fast mode (Opus only; ignored otherwise) |
| `always_workflow` | bool | `false` | Start every session with workflow mode (task callable inside code_execution) |
| `always_thinking` | bool \| string | `false` | Start every session with extended thinking (true/"adaptive", "off", an effort level ("minimal" to "max"), or a token budget) |

### `ui`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `splash_animation` | bool | `true` | - | Show splash animation on startup |
| `scrollbar` | bool | `true` | - | Show vertical scrollbar in scrollable areas |
| `flash_duration_ms` | u64 | `1500` | - | Duration of flash messages (ms) |
| `typewriter_ms_per_char` | u64 | `4` | - | Typewriter effect speed (ms/char) |
| `mouse_scroll_lines` | u32 | `3` | 1 | Lines per mouse wheel scroll |
| `show_thinking` | bool | `true` | - | When true (default), show full model reasoning live and persisted. When false, hide reasoning behind an indicator (thinking> ...) with a click-to-expand hint, both while thinking and after it completes |

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
| `max_continuation_turns` | u32 | `3` | 1 | Max automatic continuation turns |
| `compaction_buffer` | u32 \| string | `20%` | - | Context reserved for compaction: token count or percent of the context window (e.g. "20%") |

### `provider`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `default_model` | String | `none` | - | Default model identifier (e.g. `anthropic/claude-sonnet-4-6`) |
| `connect_timeout_secs` | u64 | `10` | 1 | HTTP connect timeout (seconds) |
| `low_speed_timeout_secs` | u64 | `120` | 1 | Low speed timeout (seconds with less than 1 byte received) |
| `stream_timeout_secs` | u64 | `300` | 10 | Streaming response timeout (seconds) |

### `storage`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_log_bytes_mb` | u64 | `200` | 1 | Max total log size (MB) |
| `max_log_files` | u32 | `10` | 1 | Max number of log files to keep |
| `input_history_size` | usize | `100` | 10 | Number of input history entries to retain |

## Plugins

The `plugins` table turns plugins on or off and passes options to them. All bundled plugins are on by default. Set `enabled = false` to turn one off.

Each plugin checks its own options at startup. A typo, a wrong type, or an unknown plugin name gives you a clear error right away.

The edit plugin's extra tools are options too: `plugins.edit = { multiedit = false, edit_lines = true }`. The old `tools` table is gone. If your config still uses it, Maki stops at startup and shows you the new form.

```lua
maki.setup({
    plugins = {
        bash = { timeout_secs = 180 },
        websearch = { enabled = false },
    },
})
```

### `plugins.bash`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_output_bytes` | integer | - | - | Override `agent.max_output_bytes` for this tool. |
| `max_output_lines` | integer | - | - | Override `agent.max_output_lines` for this tool. |
| `timeout_secs` | integer | `120` | 5 | Kill the command after this many seconds. A call's `timeout` param overrides it. |

### `plugins.code_execution`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_memory_mb` | integer | `50` | 10 | Memory limit for the Python sandbox (MB). |
| `max_output_bytes` | integer | - | - | Override `agent.max_output_bytes` for this tool. |
| `max_output_lines` | integer | - | - | Override `agent.max_output_lines` for this tool. |
| `timeout_secs` | integer | `30` | 5 | Stop the script after this many seconds. A call's `timeout` param overrides it. |

### `plugins.edit`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `edit_lines` | boolean | `false` | - | Provide the opt-in `edit_lines` tool. |
| `insert_lines` | boolean | `false` | - | Provide the opt-in `insert_lines` tool. |
| `multiedit` | boolean | `true` | - | Provide the `multiedit` tool. |

### `plugins.glob`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_output_bytes` | integer | - | - | Override `agent.max_output_bytes` for this tool. |
| `max_output_lines` | integer | - | - | Override `agent.max_output_lines` for this tool. |
| `search_result_limit` | integer | `100` | 10 | Max files returned per search. |

### `plugins.grep`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_line_bytes` | integer | `500` | 80 | Skip lines longer than this many bytes. |
| `max_output_bytes` | integer | - | - | Override `agent.max_output_bytes` for this tool. |
| `max_output_lines` | integer | - | - | Override `agent.max_output_lines` for this tool. |
| `search_result_limit` | integer | `100` | 10 | Max match groups per search. A call's `limit` param overrides it. |

### `plugins.index`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_file_size_mb` | integer | `2` | 1 | Refuse to index files larger than this many MB. |

### `plugins.read`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_line_bytes` | integer | `500` | 80 | Truncate lines longer than this many bytes. |
| `max_output_lines` | integer | - | - | Override `agent.max_output_lines` for this tool. |

### `plugins.skill`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `plugin_dev` | boolean | `true` | - | Offer the builtin maki-plugin-dev skill for writing maki plugins. |

### `plugins.task`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_concurrent` | integer | `8` | 1 | Max concurrently running subagents. |

### `plugins.webfetch`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_output_bytes` | integer | - | - | Override `agent.max_output_bytes` for this tool. |
| `max_output_lines` | integer | - | - | Override `agent.max_output_lines` for this tool. |
| `max_response_bytes` | integer | `5242880` | 1024 | Stop reading a response after this many bytes. |

### `plugins.websearch`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_output_bytes` | integer | - | - | Override `agent.max_output_bytes` for this tool. |
| `max_output_lines` | integer | - | - | Override `agent.max_output_lines` for this tool. |
| `max_response_bytes` | integer | `5242880` | 1024 | Stop reading a response after this many bytes. |

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

### Migrating from ~/.maki/

Older versions stored everything in `~/.maki/`. If that directory still exists, maki uses it
as a fallback. To move to XDG directories, run:

```
maki migrate xdg
```

This safely moves sessions, auth, plans, memories, logs, and preferences to XDG locations.
Where both old and new files exist, they are merged (input history, model tiers, etc.).
Nothing is deleted until it has been copied. At the end you get a summary of where everything
lives now.

Safe to run more than once.

## Personal Instructions

On top of `AGENTS.md`, you can add your own instructions in two places:

- `AGENTS.local.md` at project root for per-project preferences (gitignored)
- `~/.config/maki/AGENTS.md` for preferences that apply to all projects

Both are added to the system prompt at the start of every session.
