+++
title = "Quick Start"
weight = 1
[extra]
group = "Getting Started"
+++

# Quick Start

Install Maki, connect a provider, run a first session. A few minutes, start to finish.

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

The bash tool requires a real bash: Git for Windows, Cygwin or MSYS2. The installer will detect missing bash and offer to install Git for Windows via winget. Maki looks in the usual install locations and on PATH. If yours lives somewhere else, set `MAKI_BASH` to the full path of your `bash.exe`.

### Living on the edge (main branch)

```sh
cargo install --locked --git https://github.com/tontinton/maki.git maki
```

### With Nix

```sh
nix run github:tontinton/maki
```

Or download a pre-built binary from [GitHub Releases](https://github.com/tontinton/maki/releases/latest).

## Connect a provider

```bash
maki auth login              # interactive picker (OAuth or API key)
export ANTHROPIC_API_KEY=... # or just export a key
```

Anthropic, OpenAI, Google, Ollama, and friends all work; multiple keys in one var rotate on rate limits. Every env var and model catalog is in [Providers](/docs/providers/).

## First session

From a repo:

```bash
maki
```

Type what you want done, press Enter, watch it work. Worth knowing on day one:

- **Permissions.** File edits inside the repo run freely. `bash` and web tools ask first: `y` allows once, `s` for the session, `a` for the project. Deny rules always win; `/yolo` skips the prompts. Details in [Permissions](/docs/permissions/).
- **Plan mode.** `Tab` toggles it. The agent may only write the plan file until you approve, then back to build mode.
- **Models.** `/model` switches mid-session.
- **Sessions.** `/new` starts a second session while the first keeps working in the background; `/sessions` jumps between them. Tomorrow, `maki --continue` resumes where you left off.
- **Your shell.** Prefix input with `!` to run a command yourself (`!cargo test`). `!!` hides command and output from the agent.
- **Escape hatch.** `Esc Esc` cancels a streaming response. When idle, it rewinds instead.
- **Help.** `Ctrl+H` lists every keybinding, or see [Keybindings](/docs/keybindings/).

## Default model (optional)

```lua
-- ~/.config/maki/init.lua
maki.setup({
    provider = {
        default_model = "anthropic/claude-sonnet-4-6",
    },
})
```

Without it, Maki remembers the last model you used.

## Teach it your project

Maki loads `AGENTS.md` (or `CLAUDE.md`, `.cursorrules`, and friends) from your repo automatically. Per-project settings live under `.maki/`:

```
.maki/
├── init.lua           # overrides global config
├── permissions.toml   # permission rules
├── mcp.toml           # MCP server config
├── commands/          # custom slash commands (.md files)
└── skills/            # project skills (each dir has a SKILL.md)
AGENTS.md              # always in context
AGENTS.local.md        # personal per-project instructions (gitignored)
```

Which instruction file wins, when subdirectory rules load, and how skills and memory fit together: [Context](/docs/context/). All settings: [Configuration](/docs/configuration/).
