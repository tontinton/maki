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
      "model": "deepseek/deepseek-flash"
    },
    "type": "custom",
    "command": "maki",
    "args": ["acp"],
    "env": {}
  }
}
```

The `model` value is a `provider/model-id` spec, same format as `maki --model`.

## Flags

```bash
maki acp -m anthropic/claude-sonnet-4-6
maki acp --yolo
maki --no-jit acp
```

`maki acp` takes only `-m` / `--model` and `--yolo`. Global flags go before the subcommand: `maki --no-jit acp`, not `maki acp --no-jit`. `--allowed-tools` and `--disallowed-tools` apply to every session: `maki --allowed-tools read,grep acp`.

## What works

- **Sessions persist.** Loading a session replays the full conversation in the editor.
- **Resume picker.** The editor can list this project's stored sessions, newest first.
- **Model switching.** Pick any configured model from the editor's dropdown, mid-session. Providers that fetch their model list (like OpenRouter) fill in a few moments after the session starts.
- **Modes.** Switch between build (full access) and plan (plan-file writes only). Plan files live in the state directory as in the TUI (`…/plans/<slug>.md`), not in the SDK's `./plan.md`.
- **Permissions.** Tool permission prompts appear in the editor: allow or reject, once or always.
- **Questions.** The `question` tool becomes a native form in the editor (ACP elicitation). Without elicitation support the tool is dropped, and the model asks in plain text.
- **Live tool calls.** Tool progress streams as it happens, including subagents and batched calls.
- **Images and context.** Prompts can include images and editor-attached files.

Authentication, providers, and permissions come from your normal Maki config, so set up [providers](/docs/providers/) first.

The editor picks each session's working directory, and that folder's own
[trust](/docs/folder-trust/) decides whether its `.maki` config loads. ACP never
asks, so trust a project with `maki trust add` in it, match it with a
`trust.paths` pattern, or start the server with `maki --trust acp`.

## Sessions

One `maki acp` process serves many sessions. Zed opens each agent thread as its own session, with its own cwd, model, mode, and history.

Sessions in the folder you launched `maki acp` from run in that process. Each other folder, subfolders included, gets one child `maki acp` that serves all of its sessions. So every project loads its own plugins, config, `.env`, and `AGENTS.md`:

```
editor ── maki acp (~/code/app)             sessions in ~/code/app
              ├── maki acp (~/code/lib)     sessions in ~/code/lib
              └── maki acp (~/code/app/web) sessions in ~/code/app/web
```

Children get the same flags and log to the same stderr. They start without the variables from the launch folder's `.env`. If a child fails to start, for example on a broken `init.lua`, the editor shows the last lines of its error.

- `session/close` ends one session and cancels its running prompt.
- Loading a session that is already open reloads only that session.
- End of input closes every session. SIGINT and SIGTERM do the same, then exit with 130 or 143.

Each session starts its own [MCP](/docs/mcp/) servers: the ones configured for its cwd, plus any the editor sends. Two threads in the same project run two copies of each server.

## Commands

Type `/` in the editor's prompt to see the commands for the session:

- `/compact`, with optional guidance for the summary.
- [Custom commands](/docs/commands/#custom-commands) found for the session's cwd. `maki --no-commands acp` leaves them out.
- [MCP prompts](/docs/mcp/#prompts) of the session's servers, listed once the servers connect.

A command must start the prompt. Text, files, and images after it go along with it, except for `/compact`, which drops them. An MCP prompt missing a required argument replies with its usage instead of reaching the model.

The editor's own controls replace TUI-only commands like `/model`. Lua plugin commands are not available over ACP yet.

## Background tasks

`background = true` and `background_on_timeout` move work to the background as in the TUI. No turn starts on its own, so results arrive with the next prompt. Closing the session stops the work.

Moves are on by default. `maki --background-timeout 0 acp` turns them off, and other values change nothing in ACP. See [Background Tasks](/docs/background-tasks/#headless-sdk-and-acp).
