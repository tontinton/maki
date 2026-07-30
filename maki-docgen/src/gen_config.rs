use std::fmt::Write;
use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_config::{
    AgentConfig, ConfigField, DEFAULT_MAX_LOG_FILES, DEFAULT_MAX_OUTPUT_LINES,
    DEFAULT_MOUSE_SCROLL_LINES, MIN_TOOL_OUTPUT_LINES, ProviderConfig, StorageConfig,
    TOP_LEVEL_FIELDS, ToolOutputLines, UiConfig,
};
use maki_lua::{PluginHost, PluginOptionSpecs};

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
            ty = escape_pipes(f.ty),
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
            ty = escape_pipes(f.ty),
            desc = f.description,
        )
        .unwrap();
    }
}

fn escape_pipes(ty: &str) -> String {
    ty.replace('|', "\\|")
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

fn write_plugin_options(out: &mut String, specs: &PluginOptionSpecs) {
    for (plugin, options) in specs {
        writeln!(out, "### `plugins.{plugin}`\n").unwrap();
        writeln!(out, "| Field | Type | Default | Min | Description |").unwrap();
        writeln!(out, "|-------|------|---------|-----|-------------|").unwrap();
        for o in options {
            let default = o
                .default
                .as_ref()
                .map_or("-".to_string(), |d| format!("`{d}`"));
            let min = o.min.map_or("-".to_string(), |m| m.to_string());
            writeln!(
                out,
                "| `{name}` | {ty} | {default} | {min} | {desc} |",
                name = o.name,
                ty = o.ty,
                desc = o.desc,
            )
            .unwrap();
        }
        writeln!(out).unwrap();
    }
}

fn collect_plugin_options() -> PluginOptionSpecs {
    let host =
        PluginHost::with_all_builtins(Arc::new(ToolRegistry::new())).expect("loading builtins");
    let specs = host.plugin_options().expect("collecting plugin options");
    assert!(
        !specs.is_empty(),
        "no plugin declared options; the plugins reference would be empty"
    );
    specs
}

fn write_theme_section(out: &mut String) {
    writeln!(out, "### `ui.theme`\n").unwrap();
    writeln!(
        out,
        "Name of the color theme to load at startup, overriding the theme you \
         last picked interactively. If unset, Maki keeps your last selection \
         (the built-in default on first run). An unknown name is ignored with \
         a warning.\n"
    )
    .unwrap();
    let names = maki_ui::BUNDLED_THEMES
        .iter()
        .map(|t| format!("`{}`", t.name))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(out, "Available themes: {names}.\n").unwrap();
    writeln!(
        out,
        "Themes use 24-bit colors, but not every terminal can show them. Maki \
         checks the environment, terminfo, and the terminal itself, and when \
         truecolor is missing it quietly falls back to the closest of the 256 \
         classic terminal colors. If detection gets it wrong, set \
         `MAKI_TRUECOLOR=1` to force truecolor or `MAKI_TRUECOLOR=0` to force \
         the fallback.\n"
    )
    .unwrap();
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
        theme = \"tokyonight\",
        tool_output_lines = {{
            bash = {tol_bash},
            read = {tol_read},
        }},
    }},
    agent = {{
        max_output_lines = {max_output_lines},
    }},
    provider = {{
        default_model = \"anthropic/claude-sonnet-4-6\",
    }},
    storage = {{
        max_log_files = {max_log_files},
    }},
    plugins = {{
        bash = {{ timeout_secs = 180 }},
        index = {{ max_file_size_mb = 4 }},
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
        max_output_lines = DEFAULT_MAX_OUTPUT_LINES + 1000,
        max_log_files = DEFAULT_MAX_LOG_FILES / 2,
    )
    .unwrap();

    writeln!(out, "### Top-level\n").unwrap();
    write_table_no_min(&mut out, TOP_LEVEL_FIELDS);
    writeln!(out).unwrap();

    write_section(&mut out, "[ui]", UiConfig::FIELDS);
    write_theme_section(&mut out);
    write_tool_output_section(&mut out);
    write_section(&mut out, "[agent]", AgentConfig::FIELDS);
    write_section(&mut out, "[provider]", ProviderConfig::FIELDS);
    write_section(&mut out, "[storage]", StorageConfig::FIELDS);

    writeln!(out, "## Plugins\n").unwrap();
    writeln!(
        out,
        "The `plugins` table turns plugins on or off and passes options to \
         them. All bundled plugins are on by default. Set \
         `enabled = false` to turn one off.\n\n\
         Each plugin checks its own options at startup. A typo, a wrong \
         type, or an unknown plugin name gives you a clear error right \
         away.\n\n\
         The edit plugin's extra tools are options too: \
         `plugins.edit = {{ multiedit = false, edit_lines = true }}`. \
         The old `tools` table is gone. If your config still uses it, \
         Maki stops at startup and shows you the new form.\n"
    )
    .unwrap();
    writeln!(
        out,
        "\
```lua
maki.setup({{
    plugins = {{
        bash = {{ timeout_secs = 180 }},
        websearch = {{ enabled = false }},
    }},
}})
```\n"
    )
    .unwrap();

    write_plugin_options(&mut out, &collect_plugin_options());

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

Both are added to the system prompt at the start of every session."
    )
    .unwrap();

    out
}
