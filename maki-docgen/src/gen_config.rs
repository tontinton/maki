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

fn write_section(out: &mut String, heading: &str, fields: &[ConfigField]) {
    writeln!(out, "### `{heading}`\n").unwrap();
    if has_any_min(fields) {
        write_table_with_min(out, fields);
    } else {
        write_table_no_min(out, fields);
    }
    writeln!(out).unwrap();
}

fn write_tool_output_section(out: &mut String) {
    writeln!(out, "### `[ui.tool_output_lines]`\n").unwrap();
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

    writeln!(out, "\
+++
title = \"Configuration\"
weight = 2
[extra]
group = \"Getting Started\"
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
mouse_scroll_lines = {mouse_scroll}

[ui.tool_output_lines]
bash = {tol_bash}
read = {tol_read}

[agent]
bash_timeout_secs = {bash_timeout}
max_output_lines = {max_output_lines}

[provider]
default_model = \"anthropic/claude-sonnet-4-6\"

[storage]
max_log_files = {max_log_files}

[index]
max_file_size_mb = {max_file_size}
```

## Full Reference
",
        mouse_scroll = DEFAULT_MOUSE_SCROLL_LINES + 2,
        tol_bash = ToolOutputLines::DEFAULT.bash + 3,
        tol_read = ToolOutputLines::DEFAULT.read + 2,
        bash_timeout = DEFAULT_BASH_TIMEOUT_SECS + 60,
        max_output_lines = DEFAULT_MAX_OUTPUT_LINES + 1000,
        max_log_files = DEFAULT_MAX_LOG_FILES / 2,
        max_file_size = DEFAULT_MAX_FILE_SIZE_MB + 2,
    ).unwrap();

    writeln!(out, "### Top-level\n").unwrap();
    write_table_no_min(&mut out, TOP_LEVEL_FIELDS);
    writeln!(out).unwrap();

    write_section(&mut out, "[ui]", UiConfig::FIELDS);
    write_tool_output_section(&mut out);
    write_section(&mut out, "[agent]", AgentConfig::FIELDS);
    write_section(&mut out, "[provider]", ProviderConfig::FIELDS);
    write_section(&mut out, "[storage]", StorageConfig::FIELDS);
    write_section(&mut out, "[index]", INDEX_FIELDS);

    writeln!(out, "## Validation\n").unwrap();
    writeln!(
        out,
        "Numeric fields are validated against their minimums on load. \
         A value below the minimum raises a `ConfigError` with the section, field, value, \
         and minimum. Invalid TOML logs a warning and falls back to defaults."
    )
    .unwrap();

    writeln!(
        out,
        "
## Personal Instructions

Beyond the shared `AGENTS.md`, Maki supports two files for your own instructions:

- `AGENTS.local.md` at project root for per-project preferences (gitignored)
- `~/.maki/SYSTEM.md` for preferences that apply to all projects

Both load into the system prompt every session, right after `AGENTS.md`."
    )
    .unwrap();

    out
}
