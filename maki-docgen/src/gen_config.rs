use std::fmt::Write;

use maki_config::{
    AgentConfig, ConfigField, DEFAULT_BASH_TIMEOUT_SECS, DEFAULT_MAX_FILE_SIZE_MB,
    DEFAULT_MAX_LOG_FILES, DEFAULT_MAX_OUTPUT_LINES, DEFAULT_MOUSE_SCROLL_LINES, INDEX_FIELDS,
    MIN_TOOL_OUTPUT_LINES, ProviderConfig, StorageConfig, TOP_LEVEL_FIELDS, ToolOutputLines,
    UiConfig,
};

fn write_table_with_min(out: &mut String, fields: &[ConfigField]) {
    writeln!(out, "| Field | Type | Default | Min | Description |").unwrap();
    writeln!(out, "|-------|------|---------|-----|-------------|").unwrap();
    for f in fields {
        let default = f.default.format_default();
        let min = f.min.map_or("-".to_string(), |v| v.to_string());
        writeln!(
            out,
            "| `{name}` | {ty} | `{default}` | {min} | {desc} |",
            name = f.name,
            ty = f.ty,
            desc = f.description,
        )
        .unwrap();
    }
}

fn write_table_no_min(out: &mut String, fields: &[ConfigField]) {
    writeln!(out, "| Field | Type | Default | Description |").unwrap();
    writeln!(out, "|-------|------|---------|-------------|").unwrap();
    for f in fields {
        let default = f.default.format_default();
        writeln!(
            out,
            "| `{name}` | {ty} | `{default}` | {desc} |",
            name = f.name,
            ty = f.ty,
            desc = f.description,
        )
        .unwrap();
    }
}

fn has_any_min(fields: &[ConfigField]) -> bool {
    fields.iter().any(|f| f.min.is_some())
}

fn lua_section_name(heading: &str) -> String {
    heading
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string()
}

fn write_section(out: &mut String, heading: &str, fields: &[ConfigField]) {
    let lua_name = lua_section_name(heading);
    writeln!(out, "### `{lua_name}`\n").unwrap();
    if has_any_min(fields) {
        write_table_with_min(out, fields);
    } else {
        write_table_no_min(out, fields);
    }
    writeln!(out).unwrap();
}

fn write_tool_output_section(out: &mut String) {
    writeln!(out, "### `ui.tool_output_lines`\n").unwrap();
    writeln!(
        out,
        "How many lines of output to show per tool in the UI. \
         All values are `usize` with a minimum of {MIN_TOOL_OUTPUT_LINES}.\n"
    )
    .unwrap();
    writeln!(out, "| Field | Default |").unwrap();
    writeln!(out, "|-------|---------|").unwrap();
    for (name, default) in ToolOutputLines::FIELD_DEFAULTS {
        writeln!(out, "| `{name}` | {default} |",).unwrap();
    }
    writeln!(out).unwrap();
}

pub fn generate() -> String {
    let mut out = String::with_capacity(4096);

    writeln!(
        out,
        "\
+++
title = \"Configuration\"
weight = 2
[extra]
group = \"Getting Started\"
+++

# Configuration

Settings go in `init.lua`, a Lua script that calls `maki.setup()`. Same language as plugins.

Two places, both optional:

- **Global**: `~/.config/maki/init.lua`
- **Project**: `.maki/init.lua` (relative to your working directory)

When both exist, project settings override global ones. Neither file is required.

## Example

```lua
maki.setup({{
    ui = {{
        splash_animation = true,
        mouse_scroll_lines = {mouse_scroll},
        tool_output_lines = {{
            bash = {tol_bash},
            read = {tol_read},
        }},
    }},
    agent = {{
        bash_timeout_secs = {bash_timeout},
        max_output_lines = {max_output_lines},
    }},
    provider = {{
        default_model = \"anthropic/claude-sonnet-4-6\",
    }},
    storage = {{
        max_log_files = {max_log_files},
    }},
    index = {{
        max_file_size_mb = {max_file_size},
    }},
}})
```

All fields are optional. Typos in field names cause an error right away.

`maki.setup()` can only be called once per init.lua.

## Full Reference
",
        mouse_scroll = DEFAULT_MOUSE_SCROLL_LINES + 2,
        tol_bash = ToolOutputLines::DEFAULT.bash + 3,
        tol_read = ToolOutputLines::DEFAULT.read + 2,
        bash_timeout = DEFAULT_BASH_TIMEOUT_SECS + 60,
        max_output_lines = DEFAULT_MAX_OUTPUT_LINES + 1000,
        max_log_files = DEFAULT_MAX_LOG_FILES / 2,
        max_file_size = DEFAULT_MAX_FILE_SIZE_MB + 2,
    )
    .unwrap();

    writeln!(out, "### Top-level\n").unwrap();
    write_table_no_min(&mut out, TOP_LEVEL_FIELDS);
    writeln!(out).unwrap();

    write_section(&mut out, "[ui]", UiConfig::FIELDS);
    write_tool_output_section(&mut out);
    write_section(&mut out, "[agent]", AgentConfig::FIELDS);
    write_section(&mut out, "[provider]", ProviderConfig::FIELDS);
    write_section(&mut out, "[storage]", StorageConfig::FIELDS);
    write_section(&mut out, "[index]", INDEX_FIELDS);

    writeln!(out, "## Tools\n").unwrap();
    writeln!(
        out,
        "The `tools` table lets you turn tools on or off. \
         By default `index`, `webfetch`, and `websearch` are on. \
         `bash` is off by default.\n"
    )
    .unwrap();
    writeln!(
        out,
        "\
```lua
maki.setup({{
    tools = {{
        bash = {{ enabled = true }},
        websearch = {{ enabled = false }},
    }},
}})
```\n"
    )
    .unwrap();

    writeln!(out, "## Validation\n").unwrap();
    writeln!(
        out,
        "If a value is below its minimum, Maki shows a `ConfigError` with the field name, \
         value, and minimum."
    )
    .unwrap();

    writeln!(
        out,
        "
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
maki.setup({{
    agent = {{ bash_timeout_secs = 180 }},
}})
```

Same field names, just Lua syntax instead of TOML.

**Move MCP sections to `mcp.toml`.**

- `~/.config/maki/mcp.toml` (global)
- `.maki/mcp.toml` (per-project)

Same format, just a different file. See [MCP](/docs/mcp/).

**Permissions stay in `permissions.toml`.**"
    )
    .unwrap();

    out
}
