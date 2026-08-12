+++
title = "ACP"
weight = 22
[extra]
group = "Guides"
+++

# ACP (Agent Client Protocol)

Run Maki inside your editor. `maki acp` starts an [ACP](https://agentclientprotocol.com/) server over stdio, so any ACP-capable editor (like [Zed](https://zed.dev/)) can drive Maki as its coding agent.

```bash
maki acp
```

## Zed setup

Add Maki as a custom agent in Zed's `settings.json`:

```json
"agent_servers": {
  "Maki": {
    "default_config_options": {
      "model": "deepseek/deepseek-v4-flash"
    },
    "type": "custom",
    "command": "maki",
    "args": ["acp"],
    "env": {}
  }
}
```

The `model` value is a `provider/model-id` spec, same format as `maki --model`.

## What works

- **Sessions persist.** Loading a session replays the full conversation in the editor, so you can resume where you left off.
- **Model switching.** Pick a model from the editor's dropdown, mid-session. All configured providers show up.
- **Modes.** Switch between build (full access) and plan (plan-file writes only) from the editor.
- **Permissions.** Tool permission prompts appear in the editor: allow or reject, once or always.
- **Live tool calls.** Tool progress streams as it happens, including sub-agents and batched calls.
- **Images and context.** Prompts can include images and editor-attached files.

Authentication, providers, and permissions come from your normal Maki config. Set up [providers](/docs/providers/) first and ACP sessions just work.

```bash
maki acp
maki acp -m anthropic/claude-sonnet-4-6
maki acp --yolo
maki --no-jit acp
```

`maki acp` only takes `-m` / `--model` and `--yolo` as subcommand flags. Global flags like `--no-jit` must come before the subcommand (`maki --no-jit acp`, not `maki acp --no-jit`).

Plan mode in ACP uses the same state-directory plan files as the TUI (`…/plans/<slug>.md`), not the SDK's `./plan.md`.
