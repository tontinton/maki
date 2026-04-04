+++
title = "Providers"
weight = 5
[extra]
group = "Reference"
+++

# Providers

Maki talks to LLM providers over their HTTP APIs. Models are split into three tiers: **weak** (cheap and fast), **medium** (balanced), and **strong** (highest capability, highest cost).

## Auth Reloading

Maki re-reads auth from storage and environment variables each time a new agent spawns (`/new`, retry, session load). If you run `maki auth login` in another terminal or change an env var, the next session picks it up without a restart.

## Built-in Providers

### Anthropic

- **Env var**: `ANTHROPIC_API_KEY`
- **API**: `https://api.anthropic.com/v1/messages`
- **Features**: Prompt caching, thinking mode (adaptive/budgeted), advanced tool use

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | claude-3-5-haiku-20241022, claude-haiku-4-5-20251001, claude-3-5-haiku-latest, claude-3-haiku-20240307, **claude-haiku-4-5** (default) | $0.80 / $4.00 | 200K ctx / 8K out |
| Medium | claude-3-5-sonnet-20240620, claude-3-5-sonnet-20241022, claude-3-7-sonnet-20250219, claude-sonnet-4-5-20250929, claude-3-sonnet-20240229, claude-sonnet-4-20250514, claude-sonnet-4-0, claude-sonnet-4-5, **claude-sonnet-4-6** (default) | $3.00 / $15.00 | 200K ctx / 8K out |
| Strong | claude-opus-4-1-20250805, claude-opus-4-5-20251101, claude-3-opus-20240229, claude-opus-4-20250514, claude-opus-4-0, claude-opus-4-1, claude-opus-4-5, **claude-opus-4-6** (default) | $15.00 / $75.00 | 200K ctx / 32K out |

Defaults: claude-opus-4-6 (strong), claude-sonnet-4-6 (medium), claude-haiku-4-5 (weak)

### OpenAI

- **Env var**: `OPENAI_API_KEY` (also supports OAuth device flow)
- **API**: `https://api.openai.com/v1`

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | gpt-4.1-mini, gpt-4.1-nano, gpt-5.4-mini, **gpt-5.4-nano** (default), gpt-4o-mini, gpt-5-mini, gpt-5-nano | $0.40 / $1.60 | 1047K ctx / 32K out |
| Medium | o4-mini-deep-research, gpt-5.3-codex-spark, codex-mini-latest, **gpt-4.1** (default), o3-mini, o4-mini | $2.00 / $8.00 | 200K ctx / 100K out |
| Strong | gpt-5.1-chat-latest, gpt-5.2-chat-latest, gpt-5.3-chat-latest, gpt-5.1-codex-mini, gpt-4o-2024-05-13, gpt-4o-2024-08-06, gpt-4o-2024-11-20, gpt-5.1-codex-max, o3-deep-research, gpt-5.1-codex, gpt-5.2-codex, gpt-5.3-codex, gpt-4-turbo, gpt-5-codex, gpt-5.2-pro, gpt-5.4-pro, gpt-5-pro, gpt-5.1, gpt-5.2, **gpt-5.4** (default), gpt-4o, o1-pro, o3-pro, gpt-4, gpt-5, o1, o3 | $1.25 / $10.00 | 128K ctx / 16K out |

Defaults: gpt-5.4 (strong), gpt-4.1 (medium), gpt-5.4-nano (weak)

### Z.AI

- **Env var**: `ZHIPU_API_KEY` (shared across both endpoints)
- **API endpoints**:
  - `https://api.z.ai/api/paas/v4`
  - `https://api.z.ai/api/coding/paas/v4`

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | **glm-4.7-flash** (default), glm-4.5-flash, glm-4.5-air | $0.00 / $0.00 | 200K ctx / 131K out |
| Medium | **glm-4.7** (default), glm-4.6, glm-4.5 | $0.60 / $2.20 | 200K ctx / 131K out |
| Strong | **glm-5-code** (default), glm-5 | $1.20 / $5.00 | 200K ctx / 131K out |

Defaults: glm-5-code (strong), glm-4.7 (medium), glm-4.7-flash (weak)

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

The `base` field specifies which built-in provider to inherit the model catalog from. Valid values: `anthropic`, `openai`, `zai`, `zai-coding-plan`, `synthetic`.

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
