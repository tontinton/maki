use std::fmt::Write;
use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_config::{
    AgentConfig, ConfigField, DEFAULT_MAX_LOG_FILES, DEFAULT_MAX_OUTPUT_LINES,
    DEFAULT_MOUSE_SCROLL_LINES, MIN_TOOL_OUTPUT_LINES, NetConfig, ProviderConfig, StorageConfig,
    TOP_LEVEL_FIELDS, TelemetryConfig, ToolOutputLines, UiConfig,
};
use maki_lua::{PluginHost, PluginOptionSpecs};

type ExtraColumn = (&'static str, fn(&ConfigField) -> String);

fn write_table(out: &mut String, fields: &[ConfigField]) {
    let extra: Option<ExtraColumn> = if fields.iter().any(|f| f.env.is_some()) {
        Some(("Env", |f| {
            f.env.map_or("-".to_string(), |e| {
                e.split(", ")
                    .map(|v| format!("`{v}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
        }))
    } else if fields.iter().any(|f| f.min.is_some()) {
        Some(("Min", |f| f.min.map_or("-".to_string(), |v| v.to_string())))
    } else {
        None
    };

    let (header, rule) = extra.map_or((String::new(), ""), |(name, _)| {
        (format!(" {name} |"), "-----|")
    });
    writeln!(out, "| Field | Type | Default |{header} Description |").unwrap();
    writeln!(out, "|-------|------|---------|{rule}-------------|").unwrap();
    for f in fields {
        let cell = extra.map_or(String::new(), |(_, cell)| format!(" {} |", cell(f)));
        writeln!(
            out,
            "| `{name}` | {ty} | `{default}` |{cell} {desc} |",
            name = f.name,
            ty = escape_pipes(f.ty),
            default = f.default.format_default(),
            desc = f.description,
        )
        .unwrap();
    }
}

fn escape_pipes(ty: &str) -> String {
    ty.replace('|', "\\|")
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
    write_table(out, fields);
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
        "You can add your own themes too. Drop a `<name>.toml` file into \
         `themes/` inside your Maki config directory, for example \
         `~/.config/maki/themes/`. If it reuses a built-in name, yours wins.\n"
    )
    .unwrap();
    writeln!(
        out,
        "Diff signs use `diff_old_sign` and `diff_new_sign`, which default to \
         `diff_old` and `diff_new`. These styles are applied after `code_block`, \
         so their properties take precedence. Diff gutters use \
         `diff_old_line_nr` and `diff_new_line_nr`, which default to \
         `diff_line_nr`.\n"
    )
    .unwrap();
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

fn write_net_section(out: &mut String) {
    write_section(out, "[net]", NetConfig::FIELDS);
    writeln!(
        out,
        "`maki.net` refuses private, loopback and metadata addresses, because \
         the model picks the URLs. List a host here to let it through:\n"
    )
    .unwrap();
    writeln!(
        out,
        "\
```lua
maki.setup({{
    net = {{
        allowed_private_hosts = {{ \"localhost:8080\", \"nas.lan\", \"10.0.0.0/8\" }},
    }},
}})
```\n"
    )
    .unwrap();
    writeln!(
        out,
        "An entry with no port covers every port. A name you list is allowed \
         whatever it resolves to. A name you did not list stays blocked when \
         DNS lands it on a private address, unless that address falls in a \
         range you allowed, so keep ranges as small as the service needs. \
         Every redirect hop is checked against the same list. \
         [Permissions](/docs/permissions/#network-addresses) covers what the \
         guard protects.\n"
    )
    .unwrap();
}

fn write_telemetry_section(out: &mut String) {
    write_section(out, "[telemetry]", TelemetryConfig::FIELDS);
    writeln!(
        out,
        "Every field also has an environment variable, shown in the Env \
         column, and the variable wins. See [Telemetry](/docs/telemetry/) \
         for the full picture.\n"
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
        allowed_models = {{ \"anthropic/*\", \"openai/gpt-5\" }},
        excluded_models = {{ \"*/*-preview\" }},
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

`provider.allowed_models` is a list of glob patterns for qualified `provider/model-id` specs. `*` also matches `/`, so `opencode/*` includes nested model IDs. When the list is empty or omitted, every model is allowed. `provider.excluded_models` removes matching models after that, so exclusions always win. A project list replaces the matching global list; omit it to inherit or use `{{}}` to clear it. The policy applies to selectors, CLI and API model changes, delegation, and `maki models`.

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
    write_table(&mut out, TOP_LEVEL_FIELDS);
    writeln!(out).unwrap();

    write_section(&mut out, "[ui]", UiConfig::FIELDS);
    write_theme_section(&mut out);
    write_tool_output_section(&mut out);
    write_section(&mut out, "[agent]", AgentConfig::FIELDS);
    write_section(&mut out, "[provider]", ProviderConfig::FIELDS);
    write_section(&mut out, "[storage]", StorageConfig::FIELDS);
    write_net_section(&mut out);
    write_telemetry_section(&mut out);

    writeln!(out, "## Plugins\n").unwrap();
    writeln!(
        out,
        "The `plugins` table turns plugins on or off and passes options to \
         them. All bundled plugins are on by default. Set \
         `enabled = false` to turn one off.\n\n\
         A plugin that is off never loads, so its tool name is free for one \
         of your own plugins to take. Permission rules are keyed by the tool \
         name alone, and names such as `bash`, `write`, and `task` already \
         have rules in maki. A plugin that takes one of them inherits those \
         rules, together with any \"always allow\" you saved. Maki warns you \
         at load when this happens.\n\n\
         Each plugin checks its own options at startup. A typo, a wrong \
         type, or an unknown plugin name gives you a clear error right \
         away.\n\n\
         The edit plugin's extra tools are options too: \
         `plugins.edit = {{ multiedit = false, insert_lines = true }}`. \
         The old `tools` table is gone. If your config still uses it, \
         Maki stops at startup and shows you the new form.\n\n\
         This table is for bundled plugins only. Your own plugins go in \
         `~/.config/maki/lua/`, see [Plugins](/docs/plugins/).\n"
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

Maki follows platform directory conventions. On Linux and macOS that is XDG. On Windows, config, data, state, and logs all live under Roaming AppData (Windows has no separate state dir in this layout).

| Purpose | Linux / macOS | Windows |
|---------|---------------|---------|
| Config | `~/.config/maki/` | `%APPDATA%\\maki\\` |
| Data | `~/.local/share/maki/` | `%APPDATA%\\maki\\` |
| State | `~/.local/state/maki/` | `%APPDATA%\\maki\\` |
| Logs | `~/.local/logs/maki/` | `%APPDATA%\\maki\\` |
| Cache | `~/.cache/maki/` | `%LOCALAPPDATA%\\maki\\` |

Config holds `init.lua`, `permissions.toml`, `mcp.toml`, `providers.toml`, and `commands/`. State holds sessions, auth tokens, memories, plans, and model-tier overrides. The install script puts the binary under `%LOCALAPPDATA%\\maki` on Windows; that is separate from these runtime dirs.

`~/.maki/` (or `%USERPROFILE%\\.maki\\`) is checked as a legacy fallback. If that directory still exists, maki uses it for everything until you migrate.

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

(Linux/macOS: `~/.local/state/maki/…`; Windows: `%APPDATA%\\maki\\…`). Use them for non-obvious gotchas and decisions that should survive across sessions. They are separate from skills and from `AGENTS.md`.

Related pages: [Skills](/docs/skills/), [CLI](/docs/cli/), [Providers](/docs/providers/#providers-toml)."
    )
    .unwrap();

    out
}
