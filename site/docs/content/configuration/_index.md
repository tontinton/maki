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
        theme = "tokyonight",
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
        allowed_models = { "anthropic/*", "openai/gpt-5" },
        excluded_models = { "*/*-preview" },
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

`provider.allowed_models` is a list of glob patterns for qualified `provider/model-id` specs. `*` also matches `/`, so `opencode/*` includes nested model IDs. When the list is empty or omitted, every model is allowed. `provider.excluded_models` removes matching models after that, so exclusions always win. A project list replaces the matching global list; omit it to inherit or use `{}` to clear it. The policy applies to selectors, CLI and API model changes, delegation, and `maki models`.

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
| `notifications` | string | `auto` | - | Terminal notification method: auto, osc9, bell, or off |
| `flash_duration_ms` | u64 | `1500` | - | Duration of flash messages (ms) |
| `typewriter_ms_per_char` | u64 | `4` | - | Typewriter effect speed (ms/char) |
| `mouse_scroll_lines` | u32 | `3` | 1 | Lines per mouse wheel scroll |
| `max_input_lines` | u32 | `20` | 1 | Maximum visible input lines |
| `show_thinking` | bool | `true` | - | When true (default), show full model reasoning live and persisted. When false, hide reasoning behind an indicator (thinking> ...) with a click-to-expand hint, both while thinking and after it completes |
| `clock_format` | String | `system` | - | Clock format for timestamps: "12h", "24h", or "system" (follow the OS preference, 24h when unknown) |

### `ui.theme`

Name of the color theme to load at startup, overriding the theme you last picked interactively. If unset, Maki keeps your last selection (the built-in default on first run). An unknown name is ignored with a warning.

Available themes: `ayu_dark`, `ayu_light`, `ayu_mirage`, `carbonfox`, `catppuccin_frappe`, `catppuccin_latte`, `catppuccin_macchiato`, `catppuccin_mocha`, `dark_daltonized`, `dracula`, `everforest_dark`, `fleet_dark`, `github_dark`, `gruvbox`, `gruvbox_light`, `kanagawa`, `kanagawa_ink`, `kanagawa_plum`, `material_darker`, `monokai_pro`, `night_owl`, `nightfox`, `nord`, `onedark`, `rose_pine`, `rose_pine_dawn`, `rose_pine_midnight`, `rose_pine_moon`, `solarized_dark`, `solarized_light`, `tokyonight`, `vscode_dark_plus`, `zenburn`.

You can add your own themes too. Drop a `<name>.toml` file into `themes/` inside your Maki config directory, for example `~/.config/maki/themes/`. If it reuses a built-in name, yours wins.

Themes use 24-bit colors by default, but not every terminal can show them. Maki checks the environment, terminfo, and the terminal itself, and when truecolor is missing it quietly falls back to the closest of the 256 classic terminal colors. If detection gets it wrong, set `MAKI_TRUECOLOR=1` to force truecolor or `MAKI_TRUECOLOR=0` to force the fallback.

Theme files can also name terminal colors instead of giving hex values, using the same names as Helix: `default`, `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `gray`, `light-red`, `light-green`, `light-yellow`, `light-blue`, `light-magenta`, `light-cyan`, `light-gray`, and `white`. Write them exactly as listed. `lightgray`, `light_gray` and `LIGHT-GRAY` are all rejected. `default` means the terminal default. Maki also takes a number from `0` to `255` to pick a palette entry by index, which Helix does not.

These work everywhere a hex value does, including syntax highlighting scopes, so a theme can be written entirely against the palette your terminal already defines. Maki passes them through as palette references rather than resolving them to RGB, so the colors stay correct in terminals that mangle truecolor, such as nested tmux over ssh.

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
| `compaction_instructions` | String | `none` | - | Extra instructions appended to the compaction summary prompt |
| `post_compaction_instructions` | String | `none` | - | Extra instructions the agent receives after any compaction (e.g. re-read plan.md) |
| `stale_read_check` | bool | `true` | - | Require re-reading a file that changed on disk before editing it |
| `rtk` | bool | `true` | - | Rewrite bash commands with [rtk](https://github.com/rtk-ai/rtk) when it is installed |

### `provider`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `default_model` | String | `none` | - | Default model identifier (e.g. `anthropic/claude-sonnet-4-6`) |
| `allowed_models` | string[] | `[]` | - | Glob patterns for permitted qualified model specs; empty permits all models |
| `excluded_models` | string[] | `[]` | - | Glob patterns for excluded qualified model specs; exclusions take precedence |
| `connect_timeout_secs` | u64 | `10` | 1 | HTTP connect timeout (seconds) |
| `low_speed_timeout_secs` | u64 | `120` | 1 | Low speed timeout (seconds with less than 1 byte received) |
| `stream_timeout_secs` | u64 | `300` | 10 | Streaming response timeout (seconds) |

### `storage`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_log_bytes_mb` | u64 | `200` | 1 | Max total log size (MB) |
| `max_log_files` | u32 | `10` | 1 | Max number of log files to keep |
| `input_history_size` | usize | `100` | 10 | Number of input history entries to retain |

### `net`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `allowed_private_hosts` | string[] | `[]` | Hosts allowed to resolve to a private or loopback address, as `host`, `host:port`, or a CIDR range. Plain `http://` is kept for them instead of being upgraded to `https://` |

`maki.net` refuses private, loopback and metadata addresses, because the model picks the URLs. List a host here to let it through:

```lua
maki.setup({
    net = {
        allowed_private_hosts = { "localhost:8080", "nas.lan", "10.0.0.0/8" },
    },
})
```

An entry with no port covers every port. A name you list is allowed whatever it resolves to. A name you did not list stays blocked when DNS lands it on a private address, unless that address falls in a range you allowed, so keep ranges as small as the service needs. Every redirect hop is checked against the same list. [Permissions](/docs/permissions/#network-addresses) covers what the guard protects.

### `telemetry`

| Field | Type | Default | Env | Description |
|-------|------|---------|-----|-------------|
| `enabled` | bool | `false` | `MAKI_ENABLE_TELEMETRY` | Master switch |
| `metrics_exporter` | string | `none` | `OTEL_METRICS_EXPORTER` | Where metrics go: `otlp`, `console`, `none`, or a comma-separated mix |
| `logs_exporter` | string | `none` | `OTEL_LOGS_EXPORTER` | Where events go: `otlp`, `console`, `none`, or a comma-separated mix |
| `protocol` | string | `-` | `OTEL_EXPORTER_OTLP_PROTOCOL` | OTLP protocol: `grpc`, `http/protobuf`, or `http/json`. Required when an exporter is `otlp` |
| `endpoint` | string | `-` | `OTEL_EXPORTER_OTLP_ENDPOINT` | Collector endpoint. HTTP appends `/v1/metrics` and `/v1/logs` |
| `headers` | table | `{}` | `OTEL_EXPORTER_OTLP_HEADERS` | Extra headers sent with every export |
| `timeout_ms` | integer | `10000` | `OTEL_EXPORTER_OTLP_TIMEOUT` | Per-export request timeout (ms) |
| `compression` | string | `none` | `OTEL_EXPORTER_OTLP_COMPRESSION` | Payload compression: `gzip` or `none` |
| `metrics_protocol` | string | `-` | `OTEL_EXPORTER_OTLP_METRICS_PROTOCOL` | Metrics-only protocol override |
| `metrics_endpoint` | string | `-` | `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | Metrics-only endpoint, used verbatim with no path appended |
| `metrics_headers` | table | `{}` | `OTEL_EXPORTER_OTLP_METRICS_HEADERS` | Metrics-only headers, merged over `headers` |
| `metrics_timeout_ms` | integer | `-` | `OTEL_EXPORTER_OTLP_METRICS_TIMEOUT` | Metrics-only request timeout (ms) |
| `logs_protocol` | string | `-` | `OTEL_EXPORTER_OTLP_LOGS_PROTOCOL` | Logs-only protocol override |
| `logs_endpoint` | string | `-` | `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | Logs-only endpoint, used verbatim with no path appended |
| `logs_headers` | table | `{}` | `OTEL_EXPORTER_OTLP_LOGS_HEADERS` | Logs-only headers, merged over `headers` |
| `logs_timeout_ms` | integer | `-` | `OTEL_EXPORTER_OTLP_LOGS_TIMEOUT` | Logs-only request timeout (ms) |
| `metrics_interval_ms` | integer | `60000` | `OTEL_METRIC_EXPORT_INTERVAL` | How often metrics are exported (ms) |
| `metrics_export_timeout_ms` | integer | `30000` | `OTEL_METRIC_EXPORT_TIMEOUT` | Deadline for one metrics export, retries included (ms) |
| `logs_interval_ms` | integer | `5000` | `OTEL_LOGS_EXPORT_INTERVAL`, `OTEL_BLRP_SCHEDULE_DELAY` | How often queued events are flushed (ms) |
| `logs_max_queue_size` | integer | `2048` | `OTEL_BLRP_MAX_QUEUE_SIZE` | Event queue capacity. Events are dropped and counted when it is full |
| `logs_max_export_batch_size` | integer | `512` | `OTEL_BLRP_MAX_EXPORT_BATCH_SIZE` | Maximum events per export request |
| `logs_export_timeout_ms` | integer | `30000` | `OTEL_BLRP_EXPORT_TIMEOUT` | Deadline for one events export, retries included (ms) |
| `metrics_temporality` | string | `delta` | `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE` | Metric temporality: `delta` or `cumulative` |
| `service_name` | string | `maki` | `OTEL_SERVICE_NAME` | `service.name` on the exported resource |
| `resource_attributes` | table | `{}` | `OTEL_RESOURCE_ATTRIBUTES` | Extra resource attributes, your place for team or environment labels |
| `metrics_include_session_id` | bool | `true` | `OTEL_METRICS_INCLUDE_SESSION_ID` | Attach `session.id` to metrics. Turn off to keep metric cardinality low |
| `metrics_include_version` | bool | `false` | `OTEL_METRICS_INCLUDE_VERSION` | Attach `app.version` to metrics |
| `log_user_prompts` | bool | `false` | `OTEL_LOG_USER_PROMPTS` | Include prompt text in `maki.user_prompt` events. Off by default |
| `log_tool_details` | bool | `false` | `OTEL_LOG_TOOL_DETAILS` | Include tool input in `maki.tool_result` events. Off by default |
| `content_max_length` | integer | `10240` | `MAKI_OTEL_CONTENT_MAX_LENGTH` | Character cap on any logged prompt or tool input |

Every field also has an environment variable, shown in the Env column, and the variable wins. See [Telemetry](/docs/telemetry/) for the full picture.

## Plugins

The `plugins` table turns plugins on or off and passes options to them. All bundled plugins are on by default. Set `enabled = false` to turn one off.

A plugin that is off never loads, so its tool name is free for one of your own plugins to take. Permission rules are keyed by the tool name alone, and names such as `bash`, `write`, and `task` already have rules in maki. A plugin that takes one of them inherits those rules, together with any "always allow" you saved. Maki warns you at load when this happens.

Each plugin checks its own options at startup. A typo, a wrong type, or an unknown plugin name gives you a clear error right away.

The edit plugin's extra tools are options too: `plugins.edit = { multiedit = false, insert_lines = true }`. The old `tools` table is gone. If your config still uses it, Maki stops at startup and shows you the new form.

This table is for bundled plugins only. Your own plugins go in `~/.config/maki/lua/`, see [Plugins](/docs/plugins/).

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
| `timeout_secs` | integer | `30` | 5 | Script execution time budget in seconds; waiting on tool calls does not count. A call's `timeout` param overrides it. |

### `plugins.edit`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `edit_lines` | boolean | `true` | - | Provide the `edit_lines` tool. |
| `insert_lines` | boolean | `false` | - | Provide the opt-in `insert_lines` tool. |
| `multiedit` | boolean | `true` | - | Provide the `multiedit` tool. |

### `plugins.file_mention`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `result_limit` | integer | `10` | 1 | Max files shown in the completion popup. |

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
| `allow_model` | boolean | `false` | - | Expose a `model` input that overrides the subagent model. Only enable if you trust callers to pick an exact model themselves. |
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

Maki follows platform directory conventions. On Linux and macOS that is XDG. On Windows, config, data, state, and logs all live under Roaming AppData (Windows has no separate state dir in this layout).

| Purpose | Linux / macOS | Windows |
|---------|---------------|---------|
| Config | `~/.config/maki/` | `%APPDATA%\maki\` |
| Data | `~/.local/share/maki/` | `%APPDATA%\maki\` |
| State | `~/.local/state/maki/` | `%APPDATA%\maki\` |
| Logs | `~/.local/logs/maki/` | `%APPDATA%\maki\` |
| Cache | `~/.cache/maki/` | `%LOCALAPPDATA%\maki\` |

Config holds `init.lua`, `permissions.toml`, `mcp.toml`, `providers.toml`, and `commands/`. State holds sessions, auth tokens, memories, plans, and model-tier overrides. The install script puts the binary under `%LOCALAPPDATA%\maki` on Windows; that is separate from these runtime dirs.

`~/.maki/` (or `%USERPROFILE%\.maki\`) is checked as a legacy fallback. If that directory still exists, maki uses it for everything until you migrate.

### Migrating from ~/.maki/

```
maki migrate xdg
```

This safely moves sessions, auth, plans, memories, logs, and preferences to the platform locations above. Where both old and new files exist, they are merged (input history, model tiers, etc.). Nothing is deleted until it has been copied. At the end you get a summary of where everything lives now.

Safe to run more than once.

## Personal Instructions

On top of the project instruction files Maki loads from the git root down to the cwd (`AGENTS.md`, `CLAUDE.md`, and friends; see [Context](/docs/context/#instruction-files)), you can add:

- `AGENTS.local.md` in any of those project directories for per-directory preferences (gitignored)
- `~/.config/maki/AGENTS.md` for preferences that apply to all projects

All of these are added to the system prompt at the start of every session.

## Memory

The `memory` tool and `/memory` command store small Markdown notes under the state directory, scoped per project:

`…/state/maki/projects/<project-id>/memories/`

(Linux/macOS: `~/.local/state/maki/…`; Windows: `%APPDATA%\maki\…`). Use them for non-obvious gotchas and decisions that should survive across sessions. They are separate from skills and from `AGENTS.md`.

Related pages: [Skills](/docs/skills/), [CLI](/docs/cli/), [Providers](/docs/providers/#providers-toml).
