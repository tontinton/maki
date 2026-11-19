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

## What works

- **Sessions persist.** Loading a session replays the full conversation in the editor, so you can resume where you left off.
- **Resume picker.** Editors can list this project's stored sessions, newest first, to resume an old transcript instead of starting a blank one.
- **Model switching.** Pick a model from the editor's dropdown, mid-session. All configured providers show up. Providers that list their models over the wire (OpenRouter and friends) are discovered in the background, so the dropdown keeps filling up for a moment after the session starts, one provider at a time.
- **Modes.** Switch between build (full access) and plan (plan-file writes only) from the editor.
- **Permissions.** Tool permission prompts appear in the editor: allow or reject, once or always.
- **Questions.** The `question` tool becomes a native form in the editor (ACP elicitation). If the client does not support elicitation, the tool is dropped and the model asks in plain text.
- **Live tool calls.** Tool progress streams as it happens, including sub-agents and batched calls.
- **Images and context.** Prompts can include images and editor-attached files.

Authentication, providers, and permissions come from your normal Maki config. Set up [providers](/docs/providers/) first and ACP sessions just work.

The editor picks each session's working directory, and that folder's own
[trust](/docs/folder-trust/) decides whether its `.maki` config loads. ACP never
asks, so trust a project with `maki trust add` in it, match it with a
`trust.paths` pattern, or start the server with `maki --trust acp`.

Each folder gets its own process. Sessions in the folder you launched `maki acp`
in run in that process. A session in another folder runs in a child `maki acp`
started in that folder, so its plugins, config, `.env`, and `AGENTS.md` are its
own. A subfolder counts as another folder. One child serves every session of
its folder. Children get the same flags, and their logs go to the same stderr.
If a child dies before it is ready, for example on a broken `init.lua`, the
editor gets the last lines of its error. Children start from the environment
Maki had before it loaded the launch folder's `.env`, so values from that file
stay out of other projects. The editor still talks to one process, which passes
each message to the process that runs its session.

```bash
maki acp
maki acp -m anthropic/claude-sonnet-4-6
maki acp --yolo
maki --no-jit acp
```

`maki acp` only takes `-m` / `--model` and `--yolo` as subcommand flags. Global flags like `--no-jit` must come before the subcommand (`maki --no-jit acp`, not `maki acp --no-jit`). `--allowed-tools` and `--disallowed-tools` apply to every ACP session: `maki --allowed-tools read,grep acp`.

Plan mode in ACP uses the same state-directory plan files as the TUI (`…/plans/<slug>.md`), not the SDK's `./plan.md`.

## Sessions

One `maki acp` process serves many sessions. Zed opens each agent thread as its own session on the same process, and each one keeps its own cwd, model, mode, and history.

- `session/close` ends one session. A prompt still running there ends as cancelled, and the `SessionEnd` event gets the reason `"delete"`.
- Loading a session that is already open replaces only that session, with the reason `"replaced"`.
- End of input closes every open session. SIGINT and SIGTERM do the same, then exit with 130 or 143.

Each session starts its own [MCP](/docs/mcp/) servers: the ones configured for its cwd, plus any the editor sends. Two threads in the same project run two copies of each server.

## Commands

Type `/` in the editor's prompt to see the commands Maki offers for the session:

- `/compact`, with optional guidance for the summary.
- [Custom commands](/docs/commands/#custom-commands) found for the session's cwd. `maki --no-commands acp` leaves them out.
- [MCP prompts](/docs/mcp/#prompts) of the session's servers. The list updates once the servers connect.

A command has to start the prompt. Text after it, attached files, and images go along with the command, except for `/compact`, which drops them. An MCP prompt missing a required argument answers with its usage and does not reach the model.

TUI-only commands are not listed. The editor covers them instead: pick the model and the mode from its controls, and use its own history and theme. Lua plugin commands are not available over ACP yet.

## Background tasks

`background = true` and `background_on_timeout` move work to the background as in the TUI. No turn starts on its own, so results arrive with the next prompt. Closing the ACP session stops the work.

Moves are on unless you pass `--background-timeout 0`, which turns them off. ACP only checks whether they are on, since results arrive with the next prompt. It is a global flag: `maki --background-timeout 0 acp`. See [Background Tasks](/docs/background-tasks/#headless-sdk-and-acp).
