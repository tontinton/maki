+++
title = "Quick Start"
weight = 1
[extra]
group = "Getting Started"
+++

# Quick Start

Install Maki, connect a provider, and run your first session. Takes a few minutes.

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

Installs to `~/.local/bin`. Override with `MAKI_INSTALL_DIR`.

### Windows (PowerShell)

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

### Windows (Git Bash)

```sh
curl -fsSL https://maki.sh/install.sh | sh
```

Both install to `%LOCALAPPDATA%\maki` and add it to your user PATH. Override with `MAKI_INSTALL_DIR` / `$env:MAKI_INSTALL_DIR`.

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

These are the defaults. Plugins and `init.lua` can rebind most of them with `maki.keymap.set`; see [Keybindings](/docs/keybindings/) for precedence and caveats.

- **Newline in input**: Shift+Enter, Ctrl+Enter, Ctrl+J, or Alt+Enter
- **Scroll output**: Ctrl+U / Ctrl+D (half page)
- **Cancel streaming**: Esc Esc
- **Rewind (when idle)**: Esc Esc
- **Toggle plan / build**: Tab
- **Paste image from clipboard**: Ctrl+V (needs a vision-capable model)
- **Run a shell command yourself**: prefix the input with `!` (or `!!` to hide it from the agent). 5 minute timeout. This is your shell, not the agent `bash` tool.
- **Quit**: Ctrl+C
- **All keybindings**: Ctrl+H

## Sessions

`/new` starts another session while the previous one keeps working in the background. `/sessions` (or the session picker) jumps between them. See [Commands](/docs/commands/#sessions).

## Choosing a Model

Connect a provider first:

```bash
maki auth login          # interactive picker
# or export ANTHROPIC_API_KEY=...
```

Set a default in your config (optional; otherwise Maki remembers the last model you used):

```lua
-- ~/.config/maki/init.lua
maki.setup({
    provider = {
        default_model = "anthropic/claude-sonnet-4-6",
    },
})
```

Switch models mid-session with `/model`. See [Providers](/docs/providers/) for the full catalog and [CLI](/docs/cli/) for `maki auth` / `maki models`.

## Plan before you edit

Press `Tab` to enter plan mode. The agent may only write the plan file until you approve implementation. Press `Tab` again to return to build mode, or use the plan form when the draft is ready.

## Project Configuration

Add a `.maki/` directory to your project root for per-project settings:

```
.maki/
├── init.lua           # Overrides global config
├── permissions.toml   # Permission rules
├── mcp.toml           # MCP server config
├── commands/          # Custom slash commands (.md files)
└── skills/            # Project skills (each dir has a SKILL.md)
AGENTS.md              # Loaded into agent context automatically
AGENTS.local.md        # Personal per-project instructions (gitignored)
```

### Instruction files

At session start Maki walks from the project git root down to the working directory (if there is no `.git` root, only the cwd). In each directory it loads **one** project instruction file (first match wins), then always loads `AGENTS.local.md` if present. Closer directories win on conflicts. After that it loads one global `AGENTS.md` from your config dir:

| Order | File |
|------|------|
| 1 | `AGENTS.md` |
| 2 | `CLAUDE.md` |
| 3 | `.github/copilot-instructions.md` |
| 4 | `COPILOT.md` |
| 5 | `.cursorrules` |
| 6 | `.windsurfrules` |
| 7 | `.clinerules` |
| 8 | `CONVENTIONS.md` |
| 9 | `GEMINI.md` |
| 10 | `CODING_AGENT.md` |

Put coding conventions, repo quirks, and off-limits directories in here. When the agent `read`s under a subdirectory, Maki can also pull in that subdir's instruction file if it has not been loaded yet.

For reusable playbooks the agent loads on demand, add a skill under `.maki/skills/`. See [Skills](/docs/skills/).

See [Configuration](/docs/configuration/) for all options.
