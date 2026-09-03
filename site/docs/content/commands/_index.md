+++
title = "Commands"
weight = 8
[extra]
group = "Reference"
+++

# Commands

Type `/` in the input box to open the command palette.

## Built-in commands

| Command | Description |
|---------|-------------|
| `/compact` | Summarize and compact conversation history |
| `/new` | Start a new session |
| `/help` | Show keybindings |
| `/usage` | Show token usage breakdown |
| `/queue` | Remove items from queue |
| `/model` | Switch model |
| `/theme` | Switch color theme |
| `/mcp` | Configure MCP servers |
| `/login` | Authenticate with an LLM provider |
| `/cd` | Change working directory |
| `/btw` | Ask a quick question (no tools, no history pollution) |
| `/yolo` | Toggle YOLO mode (skip all permission prompts) |
| `/thinking` | Toggle extended thinking (off, adaptive, effort level, or budget) |
| `/fast` | Toggle Anthropic fast mode (Opus only) |
| `/workflow` | Toggle workflow mode (task callable inside code_execution) |
| `/remote-control` | Start remote control; prints a tokenized web URL |
| `/rc` | Alias of /remote-control |
| `/exit` | Exit maki |
| `/reload` | Reload plugins and config |
| `/packupdate` | Update packages (++lockfile, ! skips review) |
| `/packdel` | Remove undeclared packages (++all, or a name) |
| `/memory` | View, edit, and delete memory files |
| `/rename` | Rename the current session |
| `/sessions` | Browse and switch sessions |
| `/tasks` | Browse and search tasks |

## Sessions

Sessions run concurrently. `/new` starts a fresh session while the old one keeps working in the background, and `/sessions` shows the live status of each (working, needs input, idle) so you can jump between them. When a background session finishes or needs input, Maki flashes a note in the status bar. `/rename` renames the current session; in the session picker, `Ctrl+N` / `Ctrl+R` / `Ctrl+D` create, rename, and delete.

## Modes and toggles

- **`/yolo`**: skip permission prompts for this session (deny rules still apply). The toggle survives a resume, and `--yolo` only sets the starting value. Config: `always_yolo = true`.
- **`/thinking`**: extended thinking. Optional arg: `off`, `adaptive`, an effort level (`minimal` … `max`), or a token budget number. Config: `always_thinking`.
- **`/fast`**: Anthropic fast mode (Opus only; ignored on other models). Config: `always_fast = true`.
- **`/workflow`**: let `code_execution` call the `task` tool (and other workflow-only tools) from inside the Python sandbox. Config: `always_workflow = true`.
- **Plan / build**: not a slash command. Press `Tab` in the input to toggle plan mode (plan-file writes only).
- **`/reload`**: rebuild plugins and config without leaving the app.
- **`/btw`**: one-shot side question with no tools and no history pollution.
- **`/memory`**: open the memory file picker (view / edit / delete). See the `memory` tool under [Tools](/docs/tools/).

## Custom commands

You can define your own slash commands as Markdown files. Empty files are skipped.

### Discovery and priority

Later sources override earlier ones when the command **name** matches (the stem of the file, or `name` in frontmatter):

1. User config: `~/.config/maki/commands/` (and legacy `~/.maki/commands/` if present)
2. User third-party: `~/.claude/commands/`
3. Project dirs, walking from the current working directory up to the nearest `.git` root. At each level: `.maki/commands/`, then `.claude/commands/`

Because the walk goes cwd → … → git root, a command at the **repository root overrides** the same name found only under a nested cwd. Project commands override user commands. Palette names are `/project:<name>` or `/user:<name>` depending on which scope won.

Skip all of the above with `--no-commands` (see [CLI](/docs/cli/)).

### Metadata

You can add optional metadata at the top of the file between `---` lines to set `name`, `description`, and `argument-hint`:

```markdown
---
description: Review code for issues
argument-hint: <file>
---
Review $ARGUMENTS and suggest improvements.
```

### Arguments

Use `$ARGUMENTS` in the command body. It gets replaced with whatever you type after the command name. The command is treated as accepting args if the body contains `$ARGUMENTS` or you set `argument-hint`.

For example, `/project:review main.rs` replaces `$ARGUMENTS` with `main.rs`.

## Aliasing commands

Prefer a different name for a command? `maki.api.run_command` runs any slash command exactly as typing it would, so an alias is a one-line handler in your `init.lua` instead of a reimplementation.

```lua
-- ~/.config/maki/init.lua
local aliases = {
    { name = "/clear", target = "/new", description = "Alias for /new" },
    { name = "/resume", target = "/sessions", description = "Alias for /sessions" },
}

for _, alias in ipairs(aliases) do
    maki.api.register_command({
        name = alias.name,
        description = alias.description,
        handler = function()
            local ok, err = maki.api.run_command(alias.target)
            if not ok then
                maki.ui.flash("could not run " .. alias.target .. ": " .. err)
            end
        end,
    })
end
```

Both names stay in the palette: aliasing adds a name, it does not rename or hide the original. It works for any command listed above, plus plugin commands and MCP prompts. See [`maki.api.run_command`](/docs/lua-api/#maki-api-run_command) for matching and error handling, or [`maki.ui.action`](/docs/lua-api/#maki-ui-action) to bind a key instead of a name.

Related: [CLI](/docs/cli/) for shell flags and subcommands, [Skills](/docs/skills/) for on-demand playbooks.