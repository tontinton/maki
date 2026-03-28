+++
title = "Architecture"
weight = 8
+++

# Architecture

Maki is a Rust workspace split into 8 crates. The UI, agent logic, LLM providers, and storage each live in their own crate so they can evolve independently.

## Crate Overview

### `maki-ui`

The terminal interface, built on <a href="https://github.com/ratatui/ratatui" target="_blank">ratatui</a>. It follows an Elm-like pattern: state → view → event → update. A single-threaded event loop handles rendering and user input, then kicks off agent work on separate async tasks.

Syntax highlighting comes from <a href="https://github.com/trishume/syntect" target="_blank">syntect</a>, fuzzy search from <a href="https://github.com/helix-editor/nucleo" target="_blank">nucleo-matcher</a>. It supports inline images and clipboard access.

### `maki-agent`

The core agent loop. Runs on <a href="https://github.com/smol-rs/smol" target="_blank">`smol`</a>, sends messages to the LLM, reads responses, and executes tools as needed.

- 17 built-in tools, each with typed inputs and outputs
- A three-layer permission system: session rules, config rules, and builtin defaults (checked in that order)
- MCP client support for external tool servers
- A skill system that loads task-specific instructions
- Two operating modes: Build and Plan

### `maki-providers`

A single interface over multiple LLM HTTP APIs: Anthropic, OpenAI, Z.AI, and a Synthetic provider for testing.

Custom provider definitions placed in `~/.maki/providers/` are picked up at runtime. The crate handles streaming, token counting, retries, and prompt caching. Models are grouped into pricing tiers (weak, medium, strong) so the agent can choose appropriately.

### `maki-config`

Loads and validates TOML config files. Two layers: a global config at `~/.config/maki/config.toml` and a project-level one at `.maki/config.toml`. Project settings override global ones, field by field.

Manages `permissions.toml` and validates fields with min/max bounds. Uses <a href="https://github.com/toml-rs/toml" target="_blank">`toml_edit`</a> for writes so comments and formatting are preserved.

### `maki-storage`

Everything persistent lives under `~/.maki`: sessions, auth keys, input history, logs, theme preferences, plans.

All writes are crash-safe: write to a `.tmp` file, then atomically rename it into place. Auth files get `0600` permissions on Unix.

### `maki-code-index`

Parses source files with <a href="https://github.com/tree-sitter/tree-sitter" target="_blank">tree-sitter</a> and produces compact skeletons, typically 70-90% smaller than the original. The output keeps imports, type definitions, and function signatures with their line numbers, giving the agent enough context to navigate a codebase without reading every file in full.

Supports 15+ languages, each behind a feature gate so you only compile the grammars you need. Each language has its own `LanguageExtractor` that knows how to locate docstrings and test nodes.

### `maki-interpreter`

A Python sandbox for the `code_execution` tool. Runs on <a href="https://github.com/pydantic/monty" target="_blank">monty</a>, pydantic's minimal Python runtime, so user code is isolated from the host.

The sandbox enforces memory limits, and the agent's tools are exposed as async Python functions inside it. Input and output are JSON-serialized.

### `maki-tool-macro`

A proc-macro crate that generates the boilerplate for registering tools, parsing their parameters, and dispatching calls.

## Key Patterns

- **Async runtime**: <a href="https://github.com/smol-rs/smol" target="_blank">`smol`</a> for async, <a href="https://github.com/zesterer/flume" target="_blank">`flume`</a> channels for communication between components
- **Feature gates**: Language support in `maki-code-index` is opt-in, one feature per grammar
- **Config layering**: Project config overrides global config, merged field by field
- **Tool permissions**: Three layers checked in order: session → config → builtin
- **Atomic writes**: All persistent state uses write-to-tmp-then-rename, no half-written files
