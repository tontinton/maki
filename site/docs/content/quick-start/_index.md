+++
title = "Quick Start"
weight = 1
[extra]
group = "Getting Started"
+++

# Quick Start

## Install

Recommended way to install:

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

Living on the edge (main branch):

```sh
cargo install --locked --git https://github.com/tontinton/maki.git maki
```

With Nix:

```sh
nix run github:tontinton/maki
```

Or download a pre-built binary from [GitHub Releases](https://github.com/tontinton/maki/releases/latest).

## API Keys

Export a key for at least one provider:

| Provider | Environment Variable |
|----------|---------------------|
| Anthropic | `ANTHROPIC_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| Z.AI | `ZHIPU_API_KEY` |
| Synthetic | `SYNTHETIC_API_KEY` |

OpenAI also supports OAuth via device flow. Run `maki auth login openai` and it will walk you through setting it up.

## Run

From your project directory:

```bash
maki
```

Type a prompt, press **Enter**, and the agent starts working.

## Keybindings

- **Newline in input**: \\+Enter, Ctrl+J, or Alt+Enter
- **Scroll output**: Ctrl+U / Ctrl+D (half page), Ctrl+Y / Ctrl+E (line)
- **Cancel streaming**: Esc Esc
- **Quit**: Ctrl+C
- **All keybindings**: Ctrl+H

## Choosing a Model

Set a default in your config:

```toml
# ~/.config/maki/config.toml
[provider]
default_model = "anthropic/claude-sonnet-4-6"
```

You can also switch models mid-session with the built-in model picker.

## Project Configuration

Add a `.maki/` directory to your project root for per-project settings:

```
.maki/
├── config.toml        # Overrides global config
└── permissions.toml   # Permission rules
AGENTS.md              # Loaded into agent context automatically
AGENTS.local.md        # Personal per-project instructions (gitignored)
```

`AGENTS.md` is loaded at the start of every session. Put coding conventions, repo quirks & gotchas, or off-limits directories in here. Maki will automatically load `AGENTS.md` files inside subdirs when doing a `read` in the subdir.

See [Configuration](/docs/configuration/) for all options.
