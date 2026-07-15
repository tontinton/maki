+++
title = "Quick Start"
weight = 1
[extra]
group = "Getting Started"
+++

# Quick Start

## Install

### Linux / macOS

```sh
# Download and read the script first (don't blindly trust shell scripts).
curl -fsSL https://maki.sh/install.sh -o install.sh
cat install.sh

# Then run.
chmod +x install.sh && sh install.sh
```

One-liner:

```sh
curl -fsSL https://maki.sh/install.sh | sh
```

### Windows

```powershell
# Download and read the script first (don't blindly trust remote scripts).
irm https://maki.sh/install.ps1 -OutFile install.ps1
Get-Content install.ps1

# Then run.
.\install.ps1
```

One-liner:

```powershell
irm https://maki.sh/install.ps1 | iex
```

Installs to `%LOCALAPPDATA%\maki` and adds it to your user PATH. Override with `$env:MAKI_INSTALL_DIR`.

### Living on the edge (main branch)

```sh
cargo install --locked --git https://github.com/tontinton/maki.git maki
```

### With Nix

```sh
nix run github:tontinton/maki
```

Or download a pre-built binary from [GitHub Releases](https://github.com/tontinton/maki/releases/latest).

## API Keys

Export a key for at least one provider (e.g. `ANTHROPIC_API_KEY`). Some providers support OAuth login instead via `maki auth login <provider>`.

See [Providers](/docs/providers/) for the full list of supported providers, environment variables, and setup instructions.

## Run

From your project directory:

```bash
maki
```

Type a prompt, press **Enter**, and the agent starts working.

## Keybindings

- **Newline in input**: \\+Enter, Ctrl+J, or Alt+Enter
- **Scroll output**: Ctrl+U / Ctrl+D (half page)
- **Cancel streaming**: Esc Esc
- **Rewind (when idle)**: Esc Esc
- **Quit**: Ctrl+C
- **All keybindings**: Ctrl+H

## Choosing a Model

Set a default in your config:

```lua
-- ~/.config/maki/init.lua
maki.setup({
    provider = {
        default_model = "anthropic/claude-sonnet-4-20250514",
    },
})
```

Switch models mid-session with the `/model` command.

## Project Configuration

Add a `.maki/` directory to your project root for per-project settings:

```
.maki/
├── init.lua           # Overrides global config
├── permissions.toml   # Permission rules
├── mcp.toml           # MCP server config
└── commands/          # Custom slash commands (.md files)
AGENTS.md              # Loaded into agent context automatically
AGENTS.local.md        # Personal per-project instructions (gitignored)
```

Maki also recognizes `CLAUDE.md`, `COPILOT.md`, `.cursorrules`, `CONVENTIONS.md`, `GEMINI.md`, and others as instruction files (first match wins).

`AGENTS.md` is loaded at the start of every session. Put coding conventions, repo quirks & gotchas, or off-limits directories in here. Maki will automatically load instruction files inside subdirs when doing a `read` in the subdir.

See [Configuration](/docs/configuration/) for all options.
