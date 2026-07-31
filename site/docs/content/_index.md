+++
title = "Maki Docs"
sort_by = "weight"
+++

# Maki Docs

Maki is a terminal coding agent written in Rust. Point it at a codebase, pick an LLM provider, and it reads, edits, searches, and runs code for you while keeping token usage low.

This page is a map of the docs. Skim it once, then jump to what you need.

## Start here

New to Maki? Two pages get you going:

- [Quick Start](/docs/quick-start/) installs Maki and connects your first provider. Takes a few minutes.
- [Configuration](/docs/configuration/) covers `init.lua`, the small Lua script where all settings live.

## Everyday use

Answers to the "how do I..." questions once Maki is running:

- [Commands](/docs/commands/): `/` palette, concurrent sessions (`/new`, `/sessions`), toggles, and custom commands.
- [Keybindings](/docs/keybindings/): move around the TUI without touching the mouse. Prefix input with `!` to run a shell command yourself.
- [Tools](/docs/tools/): the full reference for the built-in tools the agent works with (including `memory` and `skill`).
- [Permissions](/docs/permissions/): which tools are gated, YOLO, and when Maki asks first.
- [Skills](/docs/skills/): on-demand Markdown playbooks the agent can load for a workflow.

## Connecting things

- [Providers](/docs/providers/): Anthropic, OpenAI, Ollama, and friends, plus `providers.toml` and model tiers.
- [MCP](/docs/mcp/): plug in external tool servers over stdio or HTTP.

## Extending and embedding

- [Lua API](/docs/lua-api/): write plugins in Lua with an API that mirrors Neovim.
- [CLI](/docs/cli/): flags and subcommands (`auth`, `models`, `acp`, `prompt`, ...).
- [Headless Mode](/docs/headless/): run Maki with `--print` in scripts and CI. Output is Claude Code compatible.
- [ACP](/docs/acp/): drive Maki from your editor, like [Zed](https://zed.dev/), over the Agent Client Protocol.

Something missing or wrong? Open an issue on [GitHub](https://github.com/tontinton/maki).
