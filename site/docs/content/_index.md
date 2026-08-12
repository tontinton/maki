+++
title = "Maki Docs"
sort_by = "weight"
+++

# Maki Docs

Maki is a terminal coding agent written in Rust, built bottom up to spend as few tokens as possible without getting dumber. Point it at a repo, pick a provider, and it reads, searches, edits, and runs code for you.

The docs are sorted by what you came here to do:

```
new to maki          ─►  Quick Start, Configuration
getting things done  ─►  Guides: Skills, Headless Mode, ACP
looking something up ─►  Reference: Tools, Providers, Permissions, ...
wondering why        ─►  Concepts: Token Economy, Context
```

## Getting started

- [Quick Start](/docs/quick-start/): install, connect a provider, first session.
- [Configuration](/docs/configuration/): `init.lua`, the small Lua script where all settings live.

## Guides

- [Skills](/docs/skills/): write Markdown playbooks the agent loads on demand.
- [Headless Mode](/docs/headless/): `--print` for scripts and CI. Drop-in Claude Code compatible.
- [ACP](/docs/acp/): drive Maki from your editor, like [Zed](https://zed.dev/), over the Agent Client Protocol.

## Concepts

How Maki thinks, worth one read:

- [Token Economy](/docs/token-economy/): where tokens go in an agent loop, and every trick Maki uses to spend fewer of them.
- [Context](/docs/context/): what enters the model's context and when, and where to put project knowledge (`AGENTS.md`, skills, memory, commands).

## Reference

Look-up material, most of it generated straight from source so it cannot drift:

- [Tools](/docs/tools/): every built-in tool and its parameters.
- [Providers](/docs/providers/): model catalogs, env vars, `providers.toml`, model tiers.
- [Permissions](/docs/permissions/): what runs freely, what asks first, TOML rules.
- [MCP](/docs/mcp/): external tool servers over stdio or HTTP.
- [Commands](/docs/commands/): the `/` palette, sessions, toggles, custom commands.
- [Keybindings](/docs/keybindings/): defaults, precedence, rebinding from Lua.
- [Lua API](/docs/lua-api/): the plugin surface, mirrored from Neovim.
- [CLI](/docs/cli/): flags and subcommands (`auth`, `models`, `acp`, `prompt`, ...).

Something missing or wrong? Open an issue on [GitHub](https://github.com/tontinton/maki).
