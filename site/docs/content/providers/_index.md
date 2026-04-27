+++
title = "Providers"
weight = 5
[extra]
group = "Reference"
+++

# Providers

Maki talks to LLM providers over their HTTP APIs. Models are split into three tiers: **weak** (cheap and fast), **medium** (balanced), and **strong** (highest capability, highest cost).

Open the model picker with `/model` and press `Alt+1`, `Alt+2`, or `Alt+3` on any row to reassign it to strong, medium, or weak. Your overrides are saved to `~/.maki/model-tiers` and apply across sessions.

## Auth Reloading

Maki re-reads auth from storage and environment variables each time a new agent spawns (`/new`, retry, session load). If you run `maki auth login` in another terminal or change an env var, the next session picks it up without a restart.

## Built-in Providers

### Anthropic

- **Env var**: `ANTHROPIC_API_KEY`
- **API**: `https://api.anthropic.com/v1/messages`
- **Features**: Prompt caching, thinking mode (adaptive/budgeted), advanced tool use

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | claude-3-haiku, claude-3-5-haiku, **claude-haiku-4-5** (default) | $0.25 / $1.25 | 200K ctx / 4K out |
| Medium | claude-3-sonnet, claude-3-5-sonnet, claude-3-7-sonnet, claude-sonnet-4, claude-sonnet-4-5, claude-sonnet-4-6-1m, **claude-sonnet-4-6** (default) | $3.00 / $15.00 | 200K ctx / 4K out |
| Strong | claude-opus-4-5, claude-opus-4-7-1m, **claude-opus-4-7** (default), claude-opus-4-6-1m, claude-opus-4-6, claude-3-opus, claude-opus-4-0, claude-opus-4-1 | $5.00 / $25.00 | 200K ctx / 64K out |

Defaults: claude-haiku-4-5 (weak), claude-sonnet-4-6 (medium), claude-opus-4-7 (strong)

### OpenAI

- **Env var**: `OPENAI_API_KEY` (also supports OAuth device flow)
- **API**: `https://api.openai.com/v1`

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | **gpt-5.4-nano** (default), gpt-5.4-mini, gpt-4.1-nano | $0.20 / $1.25 | 400K ctx / 128K out |
| Medium | gpt-4.1-mini, **gpt-4.1** (default), o4-mini, gpt-5.1-codex-mini | $0.40 / $1.60 | 1047K ctx / 32K out |
| Strong | **gpt-5.5** (default), gpt-5.4, o3, gpt-5.3-codex, gpt-5.2-codex, gpt-5.1-codex-max, gpt-5.1-codex | $5.00 / $30.00 | 1050K ctx / 128K out |

Defaults: gpt-5.4-nano (weak), gpt-4.1 (medium), gpt-5.5 (strong)

### Google

- **Env var**: `GEMINI_API_KEY`
- **API**: `https://generativelanguage.googleapis.com/v1beta`
- **Features**: Native Gemini API with thinking support

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | **gemini-2.0-flash-lite** (default) | $0.07 / $0.30 | 1048K ctx / 65K out |
| Medium | **gemini-2.5-flash** (default) | $0.15 / $0.60 | 1048K ctx / 65K out |
| Strong | **gemini-2.5-pro** (default) | $1.25 / $5.00 | 1048K ctx / 65K out |

Defaults: gemini-2.5-pro (strong), gemini-2.5-flash (medium), gemini-2.0-flash-lite (weak)

### Copilot

- **Env var**: `GH_COPILOT_TOKEN` or `~/.config/github-copilot/{hosts.json,apps.json}`
- **API**: `https://api.githubcopilot.com (or GraphQL-discovered Copilot API endpoint)`
- **Features**: Native Copilot Chat HTTP API with model endpoint discovery

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | **gpt-5-mini, gpt-5 mini, claude-haiku-4.5** (default) | $0.00 / $0.00 | 200K ctx / 100K out |
| Medium | **gpt-5.2, gpt-4.1, claude-sonnet-4.5** (default) | $0.00 / $0.00 | 200K ctx / 100K out |
| Strong | **gpt-5.4, gpt-5.3-codex, claude-opus-4.6, grok-code-fast-1** (default) | $0.00 / $0.00 | 200K ctx / 100K out |

Defaults: gpt-5-mini (weak), gpt-5.2 (medium), gpt-5.4 (strong)

### Ollama

- **Env var**: `OLLAMA_HOST` for local/remote (e.g. `http://localhost:11434`), `OLLAMA_API_KEY` for auth
- **API**: `http://localhost:11434/v1`
- **Features**: Local or remote inference via OLLAMA_HOST, cloud fallback via OLLAMA_API_KEY

This provider talks the OpenAI-compatible `/v1` API, so it also works with llama.cpp's server, LocalAI, or anything else that speaks the same protocol. Just point `OLLAMA_HOST` to the right address (e.g. `http://localhost:8080` for llama.cpp).

Maki asks the server for the list of installed models, so there's no built-in catalog. Tiers are guessed from list order: the first model becomes strong, the second medium, and the rest weak. If that guess is wrong, open `/model` and press `Alt+1`, `Alt+2`, or `Alt+3` on any row to reassign it. Your choices are saved to `~/.maki/model-tiers`.

### Mistral

- **Env var**: `MISTRAL_API_KEY`
- **API**: `https://api.mistral.ai/v1`

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | **mistral-small-latest, mistral-small-2603** (default) | $0.15 / $0.60 | 262K ctx / 262K out |
| Medium | **mistral-large-latest, mistral-large-2512** (default) | $0.50 / $1.50 | 262K ctx / 262K out |
| Strong | **devstral-latest, devstral-medium-latest, devstral-2512** (default) | $0.40 / $2.00 | 262K ctx / 262K out |

Defaults: devstral-latest (strong), mistral-large-latest (medium), mistral-small-latest (weak)

### Z.AI

- **Env var**: `ZHIPU_API_KEY` (shared across both endpoints)
- **API endpoints**:
  - `https://api.z.ai/api/paas/v4`
  - `https://api.z.ai/api/coding/paas/v4`

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | **glm-4.7-flash** (default), glm-4.5-flash, glm-4.5-air | $0.00 / $0.00 | 200K ctx / 131K out |
| Medium | **glm-4.7, glm-4.6** (default), glm-4.5 | $0.60 / $2.20 | 200K ctx / 131K out |
| Strong | **glm-5-code** (default), glm-5 | $1.20 / $5.00 | 200K ctx / 131K out |

Defaults: glm-5-code (strong), glm-4.7-flash (weak), glm-4.7 (medium)

### DeepSeek

- **Env var**: `DEEPSEEK_API_KEY`
- **API**: `https://api.deepseek.com`
- **Features**: Thinking mode toggle (on/off), open-weight models

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Medium | **deepseek-v4-flash** (default) | $0.14 / $0.28 | 1000K ctx / 384K out |
| Strong | **deepseek-v4-pro** (default) | $0.43 / $0.87 | 1000K ctx / 384K out |

Defaults: deepseek-v4-flash (medium), deepseek-v4-pro (strong)

### Synthetic

- **Env var**: `SYNTHETIC_API_KEY`
- **API**: `https://api.synthetic.new/openai/v1`
- **Features**: Reasoning effort support (low/medium/high), open-weight models

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | **hf:zai-org/GLM-4.7-Flash** (default) | $0.10 / $0.50 | 200K ctx / 131K out |
| Medium | **hf:deepseek-ai/DeepSeek-V3.2** (default) | $0.56 / $1.68 | 200K ctx / 131K out |
| Strong | **hf:moonshotai/Kimi-K2.5** (default) | $0.45 / $3.40 | 200K ctx / 131K out |

Defaults: hf:moonshotai/Kimi-K2.5 (strong), hf:deepseek-ai/DeepSeek-V3.2 (medium), hf:zai-org/GLM-4.7-Flash (weak)

## Model Identifiers

Models are referenced as `provider/model_id`:

```
anthropic/claude-sonnet-4-6
openai/gpt-4.1
zai/glm-4.7
```

If the model name is unique across providers, the prefix can be omitted.

## Dynamic Providers

To add a custom provider or proxy, drop an executable script into `~/.maki/providers/`. The script must handle these subcommands:

| Subcommand | Timeout | What it does |
|------------|---------|--------|
| `info` | 5s | Return JSON with `display_name`, `base` provider, `has_auth` |
| `models` | 5s | Return JSON array of model entries (optional) |
| `resolve` | 30s | Return auth JSON (`base_url`, `headers`) |
| `login` | interactive | OAuth or credential flow |
| `logout` | interactive | Clear credentials |
| `refresh` | 30s | Refresh auth tokens |

`resolve` is called each time a new agent spawns, so scripts should read tokens from disk instead of caching them in memory. That way auth changes from other processes get picked up.

The `base` field specifies which built-in provider to inherit the model catalog from. Valid values: `anthropic`, `openai`, `google`, `copilot`, `ollama`, `mistral`, `zai`, `zai-coding-plan`, `deepseek`, `synthetic`.

If your provider serves models not in the base catalog, add a `models` subcommand returning:

```json
[{"id": "my-model-v2", "tier": "strong", "context_window": 200000, "max_output_tokens": 16384}]
```

Only `id` is required. Optional fields: `tier` (default `medium`), `context_window` (128K), `max_output_tokens` (16K), `pricing` (`{input, output, cache_write, cache_read}`, all per 1M tokens), `supports_tool_examples` (defaults to the base provider's setting). The first model listed per tier is used for sub-agents. Without this subcommand, the base provider's models are used.

Dynamic provider models are namespaced as `{slug}/{model_id}` (e.g. `myproxy/claude-sonnet-4-6`).

### Script Name Rules

- Must start with a letter or digit
- Only letters, digits, underscores, and hyphens after that
- Can't reuse a built-in provider's slug
- Must be executable
