+++
title = "Quick Start"
weight = 1
+++

# Quick Start

## Install

```bash
cargo install maki
```

## API Keys

Export a key for at least one provider:

| Provider | Environment Variable |
|----------|---------------------|
| Anthropic | `ANTHROPIC_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| Z.AI | `ZHIPU_API_KEY` |
| Synthetic | `SYNTHETIC_API_KEY` |

OpenAI also supports OAuth via device flow. If no key is set, Maki will walk you through it on first launch.

## Run

From your project directory:

```bash
maki
```

Type a prompt, press **Enter**, and the agent starts working.

## Keybindings

- **Newline in input**: Shift+Enter, Ctrl+J, or Alt+Enter
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
├── permissions.toml   # Permission rules
└── AGENTS.md          # Loaded into agent context automatically
```

`AGENTS.md` is loaded at the start of every session. Put coding conventions, repo quirks, or off-limits directories in here.

See [Configuration](/docs/configuration/) for all options.
