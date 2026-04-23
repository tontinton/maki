+++
title = "Architecture"
weight = 9
[extra]
group = "Internals"
+++

# Architecture

Maki is a Rust workspace. The UI, agent logic, LLM providers, and storage each live in their own crate for faster compile times.

## Crate Overview

### `maki-ui`

The terminal interface, built on <a href="https://github.com/ratatui/ratatui" target="_blank">ratatui</a>. It follows an Elm-like pattern: state → view → event → update. A single-threaded event loop handles rendering and user input, then kicks off agent work on separate async tasks.

Syntax highlighting comes from <a href="https://github.com/trishume/syntect" target="_blank">syntect</a>, fuzzy search from <a href="https://github.com/helix-editor/nucleo" target="_blank">nucleo-matcher</a>. It supports inline images and clipboard access.

### `maki-agent`

The core agent loop. Runs on <a href="https://github.com/smol-rs/smol" target="_blank">`smol`</a> for faster compile times than tokio, sends messages to the LLM, reads responses, and executes tools as needed.

- 17 built-in tools, each with typed inputs and outputs
- A three-layer permission system: session rules, config rules, and builtin defaults (checked in that order)
- MCP client support for external tool servers
- A skill system that loads task-specific instructions
- Two operating modes: Build and Plan

### `maki-providers`

A single interface over multiple LLM HTTP APIs.

Custom provider definitions placed in `~/.maki/providers/` are picked up at runtime. The crate handles streaming, token counting, retries, and prompt caching. Models are grouped into pricing tiers (weak, medium, strong) so the agent can choose appropriately.

### `maki-config`

Loads and validates TOML config files. Two layers: a global config at `~/.config/maki/config.toml` and a project-level one at `.maki/config.toml`. Project settings override global ones, field by field.

Manages `permissions.toml` and validates fields with min/max bounds. Uses <a href="https://github.com/toml-rs/toml" target="_blank">`toml_edit`</a> for writes so comments and formatting are preserved.

### `maki-storage`

Everything persistent lives under `~/.maki`: sessions, auth keys, input history, logs, theme preferences, plans.

All writes are crash-safe: write to a `.tmp` file, then atomically rename it into place. Auth files get `0600` permissions on Unix.

### `maki-interpreter`

A Python sandbox for the `code_execution` tool. Runs on <a href="https://github.com/pydantic/monty" target="_blank">monty</a>, pydantic's minimal Python runtime, so user code is isolated from the host.

The sandbox enforces memory limits, and the agent's tools are exposed as async Python functions inside it. Input and output are JSON-serialized.
