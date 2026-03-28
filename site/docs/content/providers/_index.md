+++
title = "Providers"
weight = 5
+++

# Providers

Maki talks to LLM providers over their HTTP APIs. Models are split into three tiers: **weak** (cheap and fast), **medium** (balanced), and **strong** (highest capability, highest cost).

## Built-in Providers

### Anthropic

- **Env var**: `ANTHROPIC_API_KEY`
- **API**: `https://api.anthropic.com/v1/messages`
- **Features**: Prompt caching, thinking mode (adaptive/budgeted), advanced tool use

| Tier | Models |
|------|--------|
| Weak | claude-3-haiku, claude-3-5-haiku, claude-haiku-4-5 |
| Medium | claude-3-sonnet, claude-3-5-sonnet, claude-3-7-sonnet, claude-sonnet-4, claude-sonnet-4-5, claude-sonnet-4-6 |
| Strong | claude-3-opus, claude-opus-4-5, claude-opus-4-6 |

Defaults: claude-haiku-4-5 (weak), claude-sonnet-4-6 (medium), claude-opus-4-6 (strong)

### OpenAI

- **Env var**: `OPENAI_API_KEY` (also supports OAuth device flow)
- **API**: `https://api.openai.com/v1`

| Tier | Models |
|------|--------|
| Weak | gpt-5.4-nano, gpt-5.4-mini, gpt-4.1-nano |
| Medium | gpt-4.1-mini, gpt-4.1, o4-mini |
| Strong | gpt-5.4, o3 |

Defaults: gpt-5.4-nano (weak), gpt-4.1 (medium), gpt-5.4 (strong)

### Z.AI

- **Env var**: `ZHIPU_API_KEY`
- **API**: `https://api.z.ai/api/paas/v4` (standard) / `https://api.z.ai/api/coding/paas/v4` (coding)

Z.AI has two endpoints: standard and coding. The coding endpoint is used for code-specific models.

| Tier | Models |
|------|--------|
| Weak | glm-4.7-flash, glm-4.5-flash, glm-4.5-air |
| Medium | glm-4.7, glm-4.6, glm-4.5 |
| Strong | glm-5, glm-5-code |

Defaults: glm-4.7-flash (weak), glm-4.7 (medium), glm-5-code (strong)

### Synthetic

- **Env var**: `SYNTHETIC_API_KEY`
- **API**: `https://api.synthetic.new/openai/v1`
- **Features**: Reasoning effort support (low/medium/high)

Synthetic hosts open-weight models via an OpenAI-compatible API.

| Tier | Models |
|------|--------|
| Weak | hf:zai-org/GLM-4.7-Flash |
| Medium | hf:deepseek-ai/DeepSeek-V3.2 |
| Strong | hf:moonshotai/Kimi-K2.5 |

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
| `resolve` | 30s | Return auth JSON (`base_url`, `headers`) |
| `login` | interactive | OAuth or credential flow |
| `logout` | interactive | Clear credentials |
| `refresh` | 30s | Refresh auth tokens |

The `base` field specifies which built-in provider to inherit the model catalog from. Valid values: `Anthropic`, `OpenAi`, `Zai`, `ZaiCodingPlan`, `Synthetic`. For example, a proxy in front of Anthropic sets `base` to `Anthropic` and all Claude models are available, routed through your auth.

Dynamic provider models are namespaced as `{slug}/{model_id}` (e.g. `myproxy/claude-sonnet-4-6`).

### Script Name Rules

- Must start with a letter or digit
- Only letters, digits, underscores, and hyphens after that
- Can't reuse a built-in provider's slug
- Must be executable
