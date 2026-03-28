+++
title = "Maki Docs"
sort_by = "weight"
+++

# Maki

Maki is a terminal-based coding agent. Point it at a codebase, pick an LLM provider, and let it read, edit, search, and run code for you.

Written in Rust. Built to keep cost and token usage low without losing capability.

## Features

- **TUI** built on ratatui with syntax highlighting, inline image rendering, and fuzzy search.
- **17 built-in tools** for file ops, search, code execution, web access, and more.
- **Multiple providers.** Anthropic, OpenAI, Z.AI, and a dynamic provider system for plugging in your own.
- **MCP support.** Connect external tool servers over stdio or HTTP.
- **Permissions.** Fine-grained allow/deny rules, plus a YOLO mode.
- **Sub-agents.** Spin up read-only research agents or full-access workers that run in parallel.
- **Session persistence.** Pick up where you left off, context and permissions intact.
- **Python sandbox.** A minimal interpreter for running Python snippets safely inside the agent loop.
- **Code indexing.** Tree-sitter powered file skeletons for 15+ languages, so the model can understand structure without reading every line.

Ready to try it? Head to the [Quick Start](/docs/quick-start/).
