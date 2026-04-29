# maki - An efficient AI coding agent

<img src="./banner.png">

An AI coding agent optimized for minimal use of context tokens, while providing a great user experience.

## Features

### Context efficiency

* `index` tool - uses [tree-sitter](https://tree-sitter.github.io/tree-sitter) to parse supported programming languages to produce a high level skeleton of a file, with exact start-end lines of each item (e.g. a function's implementation is in lines 150-165). Encouraged to be used before reads. For my usage it adds 59 tok/turn but saves 224 tok/turn on read calls, saving 165 tok/turn.
* `code_execution` tool - uses [monty](https://github.com/pydantic/monty) to run an interpreter that has all other tools available as async functions. Maki uses it to filter / summarize / transform / pipe data to other tools as input, without it ever reaching and polluting the context window. Sandbox limited by time & memory.
* `task` tool - when delegating work to subagents, the AI chooses whether to run weak / medium / strong model of used provider. Think haiku / sonnet / opus.
* System prompt, tool descriptions, and tool examples are all concise, I've made sure not to bloat your context.
* Uses [rtk](https://github.com/rtk-ai/rtk) if you have it installed, disable with `--no-rtk`. Saves ~50% of bash output tokens. Remember bash is just 12% of total token usage, so 6% is nice, but saving on reads (65% of total) by using `index` gave me more benefit. I think I'll do bash output filtering like this myself in a future release.

### User experience

* SUPER fast startup, 60 FPS, and light on memory. Not running any javascript, using [ratatui](https://ratatui.rs) for TUI. Even the splash screen animation uses SIMD.
* Philosophy of not hiding anything - while other coding agents hide information as models improve (e.g. not showing number of lines read), maki leaves you in control.
* UI fits everything well on my small screen laptop.
* Full visibility of subagents - each subagent gets their own "chat window" you can easily navigate between using `/tasks` (Ctrl-X), or Ctrl-N/P.
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
* Plan mode.
* Run bash commands using `!`, or `!!` if you want maki to not know about it.
* `/cd` to change dir.
* Use `--print --output-format stream-json` to run UI-less. Output is compatible with Claude Code, so you can easily replace your existing solutions (although I wouldn't recommend that, maki is very new).

## Supported providers

* Anthropic - `ANTHROPIC_API_KEY` only (using OAuth is against TOS).
* OpenAI - `OPENAI_API_KEY` and OAuth via `maki auth login openai`.
* Copilot - `GH_COPILOT_TOKEN` or an existing GitHub Copilot sign-in at `~/.config/github-copilot/`.
* Ollama - `OLLAMA_HOST` for local (e.g. `http://localhost:11434`), or `OLLAMA_API_KEY` for cloud.
* Mistral - `MISTRAL_API_KEY`.
* Z.AI - `ZHIPU_API_KEY`.
* Synthetic - `SYNTHETIC_API_KEY`.

**Dynamic providers** - drop an executable script into `~/.maki/providers/` to add custom providers or proxies. See [docs](https://maki.sh/docs/providers/#dynamic-providers) for details.

## Installation

### Recommended

```sh
# Download and read the script first (don't blindly trust shell scripts).
curl -fsSL https://maki.sh/install.sh -o install.sh
cat install.sh

# Then run.
chmod +x install.sh && sh install.sh
```

### One-liner

```sh
curl -fsSL https://maki.sh/install.sh | sh
```

### Living on the edge (main branch)

```sh
cargo install --locked --git https://github.com/tontinton/maki.git maki
```

### With Nix

```sh
nix run github:tontinton/maki
```

Or download a pre-built binary from [GitHub Releases](https://github.com/tontinton/maki/releases/latest).

## Documentation

More info at the [official docs](http://maki.sh/docs).

> DISCLAIMER: >90% of code in maki was written by maki, guided by humans. The code is not as good as what I would've made in the artisinal hand-made style. But it's also not slop / vibe coded. I just think people should be honest about their use of AI in projects in this era.
