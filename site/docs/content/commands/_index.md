+++
title = "Commands"
weight = 5
[extra]
group = "Reference"
+++

# Commands

Type `/` in the input box to open the command palette.

## Built-in commands

| Command | Description |
|---------|-------------|
| `/tasks` | Browse and search tasks |
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
| `/exit` | Exit the application |
| `/reload` | Reload plugins and config |
| `/memory` | View, edit, and delete memory files |
| `/rename` | Rename the current session |
| `/sessions` | Browse and switch sessions |

## Sessions

Sessions run concurrently. `/new` starts a fresh session while the old one keeps working in the background, and `/sessions` shows the live status of each (working, needs input, idle) so you can jump between them. When a background session finishes or needs input, Maki flashes a note in the status bar.

## Custom commands

You can define your own slash commands as Markdown files.

### Project commands

Place `.md` files in `.maki/commands/` in your project root.
They appear in the palette as `/project:<filename>`.

### User commands

Place `.md` files in `~/.config/maki/commands/`.
They appear in the palette as `/user:<filename>`.

Project commands override user commands with the same name.

`.claude/commands/` directories are also supported for compatibility.

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

Use `$ARGUMENTS` in the command body. It gets replaced with whatever you type after the command name.

For example, `/project:review main.rs` replaces `$ARGUMENTS` with `main.rs`.