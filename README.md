<img src="./banner.png">

## About this fork

This fork adds remote control over the web, built for running maki on many
hosts with one shared entry point:

* **`/rc` standalone** - run `/rc` in the TUI and it prints a tokenized web URL
  (behind your own reverse proxy for TLS). The web view mirrors the live
  session: full transcript, thinking, tool calls with results, model, context
  and cost stats. You can prompt, answer permission requests, and stop runs
  from the browser.
* **`maki-anchor`** - a central server for many instances. Instances dial out
  over a WebSocket, so they need no inbound ports. One domain, one dashboard,
  one login:
  * OIDC single sign-on (Authelia, Authentik, Keycloak, Pocket ID, ...), first
    login becomes admin.
  * Per-user, per-instance grants: `viewer` (read) or `controller` (prompt and
    approve).
  * Share links with rights and expiry: `view` / `control`, default 2 hours.
  * Fleet dashboard: all instances, their sessions, costs, and status.
    Login is mandatory (first run walks you through creating the admin), and
    the link to your instance reconnects itself when the network blinks.
  * Full session transcripts are persisted on the anchor for fast reload and
    search, with per-session delete, off-the-record (OTR, never persisted),
    and a configurable pruning timeline.
* **Signed Windows binaries** - releases ship an Authenticode-signed `.exe`
  (Azure Trusted Signing), so SmartScreen stays quiet, next to static Linux
  (x86_64 + arm64) and macOS builds.

Quick start for the anchor:

```sh
# On the server: install or update maki-anchor as a systemd service.
# As root it writes a system unit; as a user, a user unit with linger.
curl -fsSL https://raw.githubusercontent.com/wmantly/maki/main/install-anchor.sh | sh

# Register an instance and copy the printed one-liner for its host.
maki-anchor tokens add work-laptop

# A CLI-minted instance has no grants yet, so it's invisible on a non-admin's
# dashboard until you grant one. Either do it in one step:
maki-anchor tokens add work-laptop --user-id 2 --rights control
# ...or grant it after the fact (see `maki-anchor users list` for ids):
maki-anchor grants set 2 work-laptop control
```

```sh
# Or skip systemd and run in the foreground
# (reverse proxy handles TLS and forwards WebSocket upgrades).
maki-anchor serve --bind 0.0.0.0:8688
```

```lua
-- In ~/.config/maki/init.lua on the instance host.
maki.setup {
  anchor = {
    url = "https://maki.example.com",
    name = "work-laptop",
    token = "<token from tokens add>",
  },
}
```

Now `/rc` prints an anchor link instead of binding a local port. See the
[anchor docs](https://maki.sh/docs/anchor/) for SSO setup, grants, and share
links.

### Screenshots

| | |
|---|---|
| ![Anchor dashboard](./screenshots/anchor-dashboard.jpg) Fleet dashboard: live shares and sessions, each with a search box over titles and full transcripts. | ![Remote terminal](./screenshots/remote-terminal.jpg) The remote terminal: full transcript, model/provider pickers, and the command toolbar. |
| ![Compact mobile view](./screenshots/remote-terminal-compact.jpg) Compact mode with the toolbar tucked away, for a phone screen. | ![QR code popup](./screenshots/remote-terminal-qr.jpg) One tap to flash the page's own link as a QR code. |

Everything below is upstream's README.

---

An AI coding agent optimized for minimal use of context tokens, while providing a great user experience.

## Features

### Context efficiency

* `index` tool - uses [tree-sitter](https://tree-sitter.github.io/tree-sitter) to parse supported programming languages to produce a high level skeleton of a file, with exact start-end lines of each item (e.g. a function's implementation is in lines 150-165). Encouraged to be used before reads. For my usage it adds 59 tok/turn but saves 224 tok/turn on read calls, saving 165 tok/turn.
* `code_execution` tool - uses [monty](https://github.com/pydantic/monty) to run an interpreter that has all other tools available as async functions. Maki uses it to filter / summarize / transform / pipe data to other tools as input, without it ever reaching and polluting the context window. Sandbox limited by time & memory.
* `task` tool - when delegating work to subagents, the AI chooses whether to run weak / medium / strong model of used provider. Think haiku / sonnet / opus.
* System prompt, tool descriptions, and tool examples are all concise, I've made sure not to bloat your context.
* Uses [rtk](https://github.com/rtk-ai/rtk) if you have it installed, disable with `maki.setup({ agent = { rtk = false } })` in your `init.lua`. Saves ~50% of bash output tokens. Remember bash is just 12% of total token usage, so 6% is nice, but saving on reads (65% of total) by using `index` gave me more benefit. I think I'll do bash output filtering like this myself in a future release.

### User experience

* SUPER fast startup, 60 FPS, and light on memory. Not running any JavaScript, using [ratatui](https://ratatui.rs) for TUI. Even the splash screen animation uses SIMD.
* Extend with neovim like Lua plugins - [Builtin plugins](https://github.com/tontinton/maki/tree/main/plugins), [User made plugins showcase](https://github.com/tontinton/maki/discussions/452), [Lua API reference](https://maki.sh/docs/lua-api/).
* Philosophy of not hiding anything - while other coding agents hide information as models improve (e.g. not showing number of lines read), maki leaves you in control.
* UI fits everything well on my small screen laptop.
* Full visibility of subagents - each subagent gets their own "chat window" you can easily navigate between using `/tasks` (Ctrl-X).
* Sensible permission system - when the agent runs `git diff && rm -rf /`, what do you think will happen in your current coding agent? It will treat it as `git *`. Maki uses tree-sitter to parse the bash command and figure out the permissions requested are `git *` and `rm *`. Disable using `--yolo`.
* SSRF protection on `webfetch` calls.
* A `memory` tool to keep long term context, just tell maki to remember something (sometimes it uses it automatically). Managed via `/memory` (view / edit / delete memories).
* Fuzzy search with Ctrl-F.
* `/btw` to run a command with the chat history without interfering with the current session.
* Rewind on Escape-Escape (no code rewind yet, only chat history).
* Attach images in prompts.
* 26 of the most popular themes.
* Resume sessions.
* Skills & MCPs.
* Opt-in [OpenTelemetry](https://maki.sh/docs/telemetry/) export, same format as Claude Code's.
* Plan mode.
* Run bash commands using `!`, or `!!` if you want maki to not know about it.
* `/cd` to change dir.
* Use `--print --output-format stream-json` to run UI-less. Output is compatible with Claude Code, so you can easily replace your existing solutions (although I wouldn't recommend that, maki is very new).

## Supported providers

* Anthropic - `ANTHROPIC_API_KEY` only (using OAuth is against TOS). Bedrock supported via `CLAUDE_CODE_USE_BEDROCK=1`.
* OpenAI - `OPENAI_API_KEY` and OAuth via `maki auth login openai`.
* xAI - `XAI_API_KEY` and OAuth via `maki auth login xai`.
* Google - `GEMINI_API_KEY`.
* Copilot - `GH_COPILOT_TOKEN` or an existing GitHub Copilot sign-in at `~/.config/github-copilot/`.
* Ollama - `OLLAMA_HOST` for local (e.g. `http://localhost:11434`), or `OLLAMA_API_KEY` for cloud.
* llama.cpp - `LLAMA_CPP_HOST` (e.g. `http://localhost:8080`), optionally `LLAMA_CPP_API_KEY`.
* Mistral - `MISTRAL_API_KEY`.
* Z.AI - `ZHIPU_API_KEY`.
* DeepSeek - `DEEPSEEK_API_KEY`.
* OpenRouter - `OPENROUTER_API_KEY`.
* Synthetic - `SYNTHETIC_API_KEY`.
* Regolo - `REGOLO_API_KEY`. EU-hosted open-weight models.
* TensorX - `TENSORX_API_KEY`.
* OpenCode Zen - `OPENCODE_API_KEY`, or the free `public` key for zero-cost models. Models from the models.dev catalog.
* OpenCode Go - `OPENCODE_API_KEY`. Models from the models.dev catalog.
* Aperture - `APERTURE_HOST` (e.g. `https://your-host.tailnet.ts.net`). No API key needed, Tailscale handles auth.

**Dynamic providers** - drop an executable script into `~/.config/maki/providers/` to add custom providers or proxies. See [docs](https://maki.sh/docs/providers/#dynamic-providers) for details.

## Installation

### Linux / macOS

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

Installs to `~/.local/bin`. Override with `MAKI_INSTALL_DIR`.

### Windows (PowerShell)

```powershell
# Download and read the script first (don't blindly trust remote scripts).
irm https://maki.sh/install.ps1 -OutFile install.ps1
Get-Content install.ps1

# Then run.
.\install.ps1
```

One-liner:

```powershell
irm https://maki.sh/install.ps1 | iex
```

### Windows (Git Bash)

```sh
curl -fsSL https://maki.sh/install.sh | sh
```

Both install to `%LOCALAPPDATA%\maki` and add it to your user PATH. Override with `MAKI_INSTALL_DIR` / `$env:MAKI_INSTALL_DIR`.

### Living on the edge (main branch)

```sh
cargo install --locked --git https://github.com/tontinton/maki.git maki
```

### With Nix

```sh
nix run github:tontinton/maki
```

Or download a pre-built binary from [GitHub Releases](https://github.com/tontinton/maki/releases/latest).

## ACP

Run `maki acp` or configure your ACP supporting editor to use maki, e.g. in [Zed](https://zed.dev/)'s `settings.json`:

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

## Documentation

More info at the [official docs](https://maki.sh/docs).

## Community

[![Discord](https://img.shields.io/discord/1543246528876126218?logo=discord&logoColor=white&label=discord&color=5865F2)](https://discord.gg/dEBhANTbX)

## Example config

[tontinton/makiconf](https://github.com/tontinton/makiconf) - includes a [semble](https://github.com/MinishLab/semble) tool (Lua code) for semantic code search, and an [ast-grep](https://ast-grep.github.io) MCP server for AST-based search and replace.

> DISCLAIMER: >90% of code in maki was written by maki, guided by humans. Some parts of the code are not as good as what I would've made in the artisanal hand-made style. But it's also not slop / vibe coded, and can easily be refactored if needed nowadays.
