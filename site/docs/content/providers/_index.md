+++
title = "Providers"
weight = 5
[extra]
group = "Reference"
+++

# Providers

Maki talks to LLM providers over their HTTP APIs. Models are split into three tiers: **weak** (cheap and fast), **medium** (balanced), and **strong** (highest capability, highest cost). There is also a **compaction** tier for choosing a dedicated model to summarize context when the conversation grows long.

Open the model picker with `/model` and press `!`, `@`, `#`, or `$` on any row to assign it to strong, medium, weak, or compaction. Press the same key again to remove the assignment. Your overrides are saved to `~/.local/state/maki/model-tiers` and apply across sessions.

## Auth Reloading

Maki re-reads auth from storage and environment variables each time a new agent spawns (`/new`, retry, session load). If you run `maki auth login` in another terminal or change an env var, the next session picks it up without a restart.

You can set multiple API keys in one env var (`ANTHROPIC_API_KEY=sk-1,sk-2,sk-3`). On a rate-limit or auth error, maki switches to the next key right away, with no delay and without spending a retry. Each request walks the pool once before falling back to normal backoff. A plan quota error does not rotate keys, since the quota is per account and the others are just as spent.

## Base URL Overrides

Every provider honors a `<SLUG>_BASE_URL` env var (`anthropic` -> `ANTHROPIC_BASE_URL`, `llama-cpp` -> `LLAMA_CPP_BASE_URL`). Set it to the origin of a proxy or a compatible endpoint and Maki appends the API paths itself:

```sh
ANTHROPIC_BASE_URL=https://my-proxy.internal maki
```

Built-in, plugin and `providers.toml` providers all read it. When more than one origin is available, the first of these that is set wins:

1. An origin returned by the provider's auth hook, which is how a login flow points the provider at the endpoint it was given.
2. `<SLUG>_BASE_URL`.
3. `base_url` in `providers.toml`.
4. The `base_url` the provider's own declaration carries.

`ANTHROPIC_BASE_URL` and `OPENAI_BASE_URL` are the same names the official SDKs use, so an existing proxy setup carries over as is. Two exceptions: `OPENAI_BASE_URL` only redirects the platform API, never the ChatGPT Coding Plan backend; `XAI_BASE_URL` only redirects the public API-key endpoint, never the OAuth CLI proxy.

You can also set `base_url` for a built-in provider in `~/.config/maki/providers.toml`. It overrides the built-in default and loses to the env var above:

```toml
[openai]
base_url = "http://xxxx:1234/v1"
```

The built-in provider still owns the slug, so `protocol`, `api_key_env`, `discover_models` and `models` are ignored with a warning. Use a custom slug if you need those.

## Built-in Providers

### Anthropic

- **Env var**: `ANTHROPIC_API_KEY`
- **API**: `https://api.anthropic.com/v1/messages`
- **Features**: Prompt caching, thinking mode (adaptive/budgeted), advanced tool use

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | **claude-haiku-4-5** (default) | $1.00 / $5.00 | 200K ctx / 64K out |
| Medium | claude-sonnet-4-5 | $3.00 / $15.00 | 200K ctx / 64K out |
| Medium | claude-sonnet-4-6 | $3.00 / $15.00 | 200K ctx / 64K out |
| Medium | **claude-sonnet-5** (default) | $2.00 / $10.00 | 200K ctx / 128K out |
| Medium | claude-sonnet-4 | $3.00 / $15.00 | 200K ctx / 64K out |
| Strong | claude-opus-4-5 | $5.00 / $25.00 | 200K ctx / 64K out |
| Strong | claude-opus-4-6 | $5.00 / $25.00 | 200K ctx / 128K out |
| Strong | claude-opus-4-7 | $5.00 / $25.00 | 200K ctx / 128K out |
| Strong | claude-opus-4-8 | $5.00 / $25.00 | 200K ctx / 128K out |
| Strong | **claude-opus-5** (default) | $5.00 / $25.00 | 200K ctx / 128K out |
| Strong | claude-fable-5 | $10.00 / $50.00 | 200K ctx / 128K out |
| Strong | claude-opus-4-0, claude-opus-4-1 | $15.00 / $75.00 | 200K ctx / 32K out |

Defaults: claude-haiku-4-5 (weak), claude-sonnet-5 (medium), claude-opus-5 (strong)

Add `-1m` to any Claude model, like `claude-sonnet-4-6-1m`, to use the 1M token context window.

#### Amazon Bedrock

If you already use Claude through AWS Bedrock, you can point Maki at it instead of the direct Anthropic API. Set `CLAUDE_CODE_USE_BEDROCK=1` and Maki will route all Anthropic requests through Bedrock. The same models, the same features, just a different door.

You will need `AWS_REGION` and one of the following for auth:

| Method | Env vars |
|--------|----------|
| IAM credentials | `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (and optionally `AWS_SESSION_TOKEN`) |
| Credentials file | `AWS_PROFILE` (defaults to `default`), reads `~/.aws/credentials` |
| Bearer token | `AWS_BEARER_TOKEN_BEDROCK` |
| Gateway proxy | `CLAUDE_CODE_SKIP_BEDROCK_AUTH=1` + `ANTHROPIC_BEDROCK_BASE_URL` (skips signing, useful behind a proxy that handles auth) |

You can override the model with `ANTHROPIC_MODEL` and the endpoint with `ANTHROPIC_BEDROCK_BASE_URL`. These env var names match Claude Code, so if you were already using Bedrock there, the same setup works here.

### OpenAI

- **Env var**: `OPENAI_API_KEY` (also supports OAuth via `maki auth login openai`)
- **API**: `https://api.openai.com/v1`

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | **gpt-5.6-luna** (default) | $1.00 / $6.00 | 372K ctx / 128K out |
| Weak | gpt-6-luna | $0.10 / $0.50 | 1050K ctx / 128K out |
| Weak | gpt-5.4-nano | $0.20 / $1.25 | 400K ctx / 128K out |
| Weak | gpt-5.4-mini | $0.75 / $4.50 | 400K ctx / 128K out |
| Weak | gpt-4.1-nano | $0.10 / $0.40 | 1047K ctx / 32K out |
| Medium | **gpt-5.6-terra** (default) | $2.50 / $15.00 | 372K ctx / 128K out |
| Medium | gpt-6-sol | $2.00 / $10.00 | 1050K ctx / 128K out |
| Medium | gpt-4.1-mini | $0.40 / $1.60 | 1047K ctx / 32K out |
| Medium | gpt-4.1 | $2.00 / $8.00 | 1047K ctx / 32K out |
| Medium | o4-mini | $1.10 / $4.40 | 200K ctx / 100K out |
| Medium | gpt-5.1-codex-mini | $0.25 / $2.00 | 400K ctx / 128K out |
| Strong | **gpt-5.6-sol** (default) | $5.00 / $30.00 | 372K ctx / 128K out |
| Strong | gpt-6-astra | $10.00 / $50.00 | 1050K ctx / 128K out |
| Strong | gpt-5.5 | $5.00 / $30.00 | 1050K ctx / 128K out |
| Strong | gpt-5.4 | $2.50 / $15.00 | 1050K ctx / 128K out |
| Strong | o3 | $2.00 / $8.00 | 200K ctx / 100K out |
| Strong | gpt-5.3-codex | $1.75 / $14.00 | 400K ctx / 128K out |
| Strong | gpt-5.2-codex | $1.75 / $14.00 | 400K ctx / 128K out |
| Strong | gpt-5.1-codex-max | $1.25 / $10.00 | 400K ctx / 128K out |
| Strong | gpt-5.1-codex | $1.25 / $10.00 | 400K ctx / 128K out |

Defaults: gpt-5.6-luna (weak), gpt-5.6-terra (medium), gpt-5.6-sol (strong)

`maki auth login openai` offers browser login (PKCE, callback on `localhost:1455`) and device code login. Browser is the desktop default; device code is recommended over SSH or in a container. Tokens refresh automatically.

With ChatGPT OAuth the model list comes from the Codex backend's own `/models` endpoint, so a model your plan gains shows up without a Maki update, with the context window and reasoning levels the backend declares for it. The table above is the offline fallback. The endpoint hides models newer than the Codex CLI version Maki reports, so a brand new release can lag until that version is bumped.

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

- **Env var**: `GH_COPILOT_TOKEN` (or run `maki auth login copilot` to import a token from gh CLI, the Copilot client, or the system keyring)
- **API**: `https://api.githubcopilot.com (or GraphQL-discovered Copilot API endpoint)`
- **Features**: Native Copilot Chat HTTP API with model endpoint discovery

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | gpt-5-mini | $0.25 / $2.00 | 200K ctx / 100K out |
| Weak | gpt-5.4-mini | $0.75 / $4.50 | 200K ctx / 100K out |
| Weak | gpt-5.4-nano | $0.20 / $1.25 | 200K ctx / 100K out |
| Weak | claude-haiku-4.5 | $1.00 / $5.00 | 200K ctx / 64K out |
| Weak | gemini-3.5-flash | $1.50 / $9.00 | 200K ctx / 65K out |
| Weak | mai-code-1-flash-picker | $0.75 / $4.50 | 200K ctx / 100K out |
| Weak | **gpt-5.6-luna** (default) | $0.20 / $1.20 | 200K ctx / 100K out |
| Medium | gemini-3.6-flash | $0.75 / $3.75 | 200K ctx / 65K out |
| Medium | gemini-3.7-flash | $0.75 / $3.75 | 200K ctx / 65K out |
| Medium | claude-sonnet-4.5, claude-sonnet-4.6 | $3.00 / $15.00 | 200K ctx / 64K out |
| Medium | claude-sonnet-5 | $2.00 / $10.00 | 200K ctx / 100K out |
| Medium | kimi-k2.7-code | $0.95 / $4.00 | 200K ctx / 100K out |
| Medium | gemini-3.1-pro-preview | $2.00 / $12.00 | 200K ctx / 65K out |
| Medium | **gpt-5.6-terra** (default) | $2.00 / $12.00 | 200K ctx / 100K out |
| Medium | grok-4.5 | $2.00 / $6.00 | 200K ctx / 100K out |
| Medium | grok-4.6 | $2.00 / $6.00 | 200K ctx / 100K out |
| Strong | gpt-5.5 | $5.00 / $30.00 | 200K ctx / 100K out |
| Strong | kimi-k3 | $3.00 / $15.00 | 200K ctx / 100K out |
| Strong | gpt-5.4 | $2.50 / $15.00 | 200K ctx / 100K out |
| Strong | gpt-5.6-sol | $5.00 / $30.00 | 200K ctx / 100K out |
| Strong | gpt-5.3-codex | $1.75 / $14.00 | 200K ctx / 100K out |
| Strong | **claude-opus-5, claude-opus-4.8, claude-opus-4.7, claude-opus-4.6, claude-opus-4.5** (default) | $5.00 / $25.00 | 200K ctx / 64K out |
| Strong | claude-opus-4.8-fast, claude-fable-5 | $10.00 / $50.00 | 200K ctx / 100K out |

Defaults: gpt-5.6-luna (weak), gpt-5.6-terra (medium), claude-opus-5 (strong)

### Ollama

- **Env var**: `OLLAMA_HOST` for local/remote (e.g. `http://localhost:11434`), `OLLAMA_API_KEY` for auth
- **API**: `http://localhost:11434/v1`
- **Features**: Local or remote inference via OLLAMA_HOST, cloud fallback via OLLAMA_API_KEY

This provider talks the OpenAI-compatible `/v1` API, so it also works with llama.cpp's server, LocalAI, or anything else that speaks the same protocol. Just point `OLLAMA_HOST` to the right address (e.g. `http://localhost:8080` for llama.cpp).

### LlamaCpp

- **Env var**: `LLAMA_CPP_API_KEY`
- **API**: `http://localhost:8080/v1`
- **Features**: Local or remote inference via LLAMA_CPP_HOST, set optional key via LLAMA_CPP_API_KEY

Connects to any OpenAI-compatible `/v1` endpoint. Point `LLAMA_CPP_HOST` to your server address (defaults to `http://localhost:8080`).

### Mistral

- **Env var**: `MISTRAL_API_KEY`
- **API**: `https://api.mistral.ai/v1`

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | **ministral-14b-latest, ministral-14b-2512** (default) | $0.20 / $0.20 | 262K ctx |
| Medium | **mistral-small-latest, mistral-small-2603** (default) | $0.15 / $0.60 | 262K ctx |
| Strong | **mistral-medium-latest, mistral-medium-3.5, mistral-medium-3-5, mistral-medium-2604** (default) | $1.50 / $7.50 | 262K ctx |
| Strong | zai-glm-latest, zai-glm-5-3, zai-glm-5 | $1.40 / $4.40 | 1000K ctx |
| Strong | glm-5-2, zai-glm-5-2 | $1.40 / $4.40 | 1000K ctx |

Defaults: mistral-medium-latest (strong), mistral-small-latest (medium), ministral-14b-latest (weak)

### Z.AI

- **Env var**: `ZHIPU_API_KEY` (shared across both endpoints)
- **API endpoints**:
  - `https://api.z.ai/api/paas/v4`
  - `https://api.z.ai/api/coding/paas/v4`

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | glm-5.3-flash | $0.15 / $0.50 | 1000K ctx / 131K out |
| Weak | **glm-4.7-flash** (default) | $0.00 / $0.00 | 200K ctx / 131K out |
| Weak | glm-4.5-flash | $0.00 / $0.00 | 131K ctx / 98K out |
| Weak | glm-4.5-air | $0.20 / $1.10 | 131K ctx / 98K out |
| Medium | **glm-4.7, glm-4.6** (default) | $0.60 / $2.20 | 200K ctx / 131K out |
| Medium | glm-4.5 | $0.60 / $2.20 | 131K ctx / 98K out |
| Strong | **glm-5-code** (default) | $1.20 / $5.00 | 200K ctx / 131K out |
| Strong | glm-5.3 | $1.40 / $4.40 | 1000K ctx / 131K out |
| Strong | glm-5.2 | $1.40 / $4.40 | 1000K ctx / 131K out |
| Strong | glm-5.1 | $1.40 / $4.40 | 200K ctx / 131K out |
| Strong | glm-5 | $1.00 / $3.20 | 200K ctx / 131K out |

Defaults: glm-5-code (strong), glm-4.7-flash (weak), glm-4.7 (medium)

### DeepSeek

- **Env var**: `DEEPSEEK_API_KEY`
- **API**: `https://api.deepseek.com`
- **Features**: Thinking mode toggle (on/off), open-weight models
- **Peak pricing**: the prices below are off-peak; each turn is billed as it happens, at 2x during 01:00-04:00, 06:00-10:00 UTC, Mon-Fri

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Medium | **deepseek-flash, deepseek-v4-flash** (default) | $0.15 / $0.60 | 1000K ctx / 384K out |
| Strong | **deepseek-v4-pro** (default) | $0.66 / $1.98 | 1000K ctx / 384K out |

Defaults: deepseek-flash (medium), deepseek-v4-pro (strong)

### OpenRouter

- **Env var**: `OPENROUTER_API_KEY`
- **API**: `https://openrouter.ai/api/v1`
- **Features**: 300+ models from all providers, prompt caching, provider routing

OpenRouter aggregates models from many providers behind a single API key. Browse available models at [openrouter.ai/models](https://openrouter.ai/models). Use any model ID directly (e.g. `openrouter/anthropic/claude-sonnet-4`).

### Requesty

- **Env var**: `REQUESTY_API_KEY`
- **API**: `https://router.requesty.ai/v1`
- **Features**: 700+ models behind one key, curated managed routing policies, EU region via `REQUESTY_BASE_URL`

Requesty routes 700+ models from many providers behind a single API key. Models are listed live from the API: curated managed policies first (short ids such as `requesty/claude-sonnet-4-5` or `requesty/gpt-5.4-mini`, `@eu` variants route only through EU providers), then the full `<vendor>/<model>` catalog (e.g. `requesty/openai/gpt-4o-mini`). Get a key at [app.requesty.ai/api-keys](https://app.requesty.ai/api-keys). Set `REQUESTY_BASE_URL=https://router.eu.requesty.ai/v1` to keep all traffic in the EU.

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

### Regolo

- **Env var**: `REGOLO_API_KEY`
- **API**: `https://api.regolo.ai/v1`
- **Features**: EU-hosted open-weight models with tool calling. The catalogue and prices are listed live from the API

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Weak | **qwen3.5-9b** (default) | $0.07 / $0.35 | 80K ctx / 80K out |
| Medium | **qwen3-coder-next** (default) | $0.50 / $2.00 | 120K ctx / 120K out |
| Strong | **qwen3.5-122b** (default) | $1.00 / $4.20 | 120K ctx / 120K out |

Defaults: qwen3.5-122b (strong), qwen3-coder-next (medium), qwen3.5-9b (weak)

### TensorX

- **Env var**: `TENSORX_API_KEY`
- **API**: `https://api.tensorx.ai/v1`
- **Features**: Open-weight models, zero data retention, prompt caching

No hardcoded model catalog. Use any model ID supported by this provider.

### Opencode Zen

- **Env var**: `OPENCODE_API_KEY`
- **API**: `https://opencode.ai/zen/v1`
- **Features**: Dynamically discovered models via [models.dev](https://models.dev/) + all the models provided by Opencode Zen API

No hardcoded model catalog. Use any model ID supported by this provider.

By default Maki hides free models from the Opencode catalog. To list free models (they use a public fallback, no API key needed), add this to `~/.config/maki/providers.toml`:

```toml
[opencode]
enable_free_models = true
```

The default is `false`.

### xAI

- **Env var**: `XAI_API_KEY` (also supports OAuth via `maki auth login xai`)
- **API endpoints**:
  - `https://api.x.ai/v1`
  - `https://cli-chat-proxy.grok.com/v1`
- **Features**: OAuth login, account-specific model catalog, Grok reasoning (low/medium/high/xhigh)

| Tier | Models | Pricing (in/out per 1M tokens) | Context |
|------|--------|-------------------------------|---------|
| Medium | **grok-4.3** (default) | $1.25 / $2.50 | 1000K ctx / 131K out |
| Strong | **grok-4.6** (default) | $2.00 / $6.00 | 500K ctx / 131K out |
| Strong | grok-4.5 | $2.00 / $6.00 | 500K ctx / 131K out |

Defaults: grok-4.6 (strong), grok-4.3 (medium)

OAuth uses the same first-party xAI client as the official Grok CLI (`maki auth login xai`). Browser login (PKCE) is the desktop default; device code is recommended over SSH or in a container. Tokens refresh automatically. After login, Maki fetches your account catalog from `GET /v1/models-v2` on the Grok CLI proxy and caches it for 15 minutes. `XAI_BASE_URL` only redirects the public API-key endpoint, never the OAuth proxy.

If `~/.grok/auth.json` already exists, login offers to reuse it without writing that file.

### Aperture

- **Env var**: `APERTURE_HOST` (e.g. `https://your-host.tailnet.ts.net`)
- **API**: `Aperture gateway (set APERTURE_HOST)`
- **Features**: Tailscale Aperture LLM gateway; set APERTURE_HOST or configure in providers.toml

Aperture discovers models from your gateway. Set `APERTURE_HOST` to your Tailscale Aperture endpoint (e.g. `https://your-host.tailnet.ts.net`). No API key needed, Tailscale handles auth.

### Opencode Go

- **Env var**: `OPENCODE_API_KEY`
- **API**: `https://opencode.ai/zen/go/v1`
- **Features**: Dynamically discovered models via [models.dev](https://models.dev/) + all the models provided by Opencode Go API

No hardcoded model catalog. Use any model ID supported by this provider. An API key is required.

## Model Identifiers

Models are referenced as `provider/model_id`:

```
anthropic/claude-sonnet-4-6
openai/gpt-4.1
xai/grok-4.6
zai/glm-4.7
```

If the model name is unique across providers, the prefix can be omitted.

### Models newer than your Maki version

The tables above list the models Maki curates. Any other id a provider accepts works too: type it into `/model` or pass it to `--model`. The picker also lists what the provider's own model endpoint reports, so same-day releases are selectable there.

For an id no table covers, rates, context window, vision and thinking support come from [models.dev](https://models.dev/), refreshed daily (`maki models --refresh` forces it). Maki reads each field on its own, so a row that lists a price but no context window still leaves the window to the sources below.

Sources rank by how sure they are to describe the exact model you asked for:

1. What the provider's own model endpoint reported this session.
2. A curated row for that id, including its dated snapshots. `claude-sonnet-4-5-20250929` reads the `claude-sonnet-4-5` row.
3. models.dev.
4. A curated row for a close relative, reached by shared prefix. `glm-5.4` falls back to `glm-5` here, and takes its family and tier from it either way.
5. The provider's defaults, with no cost estimate.

A curated row is checked against the provider's own pricing page, so it wins for the id it names. For a relative it loses to models.dev, because a rate nobody checked against the id you typed is only a guess.

New models start at the **medium** tier until you assign one in the picker.

## providers.toml

`providers.toml` lives in the config directory (`~/.config/maki/providers.toml` on Linux/macOS, `%APPDATA%\maki\providers.toml` on Windows). It is the file for provider overrides and custom HTTP providers. Two jobs:

1. Tweak a built-in (pick a plan, change its base URL, set `enable_free_models` for Opencode).
2. Declare a custom provider that speaks OpenAI, Anthropic, or Google wire format.

```toml
# Point a built-in at a proxy. Env vars still win over this file.
[anthropic]
base_url = "https://my-proxy.internal"

# Full custom provider. Slug becomes the `provider/` prefix in model specs.
[my-proxy]
display_name = "My Proxy"
protocol = "openai"            # openai | openai-responses | anthropic | google
base_url = "https://llm.example.com/v1"
api_key_env = "MY_PROXY_API_KEY"
default_model = "my-proxy/fast-v1"
discover_models = true         # also list models via the provider's /models endpoint

[[my-proxy.models]]
id = "fast-v1"
tier = "weak"
context_window = 128000
max_output_tokens = 16384
pricing_input = 0.5
pricing_output = 1.5

[[my-proxy.models]]
id = "smart-v1"
tier = "strong"
context_window = 200000
max_output_tokens = 32000
supports_thinking = true
supports_vision = false
```

### Provider fields

| Field | Type | Notes |
|-------|------|-------|
| `display_name` | string | Shown in pickers and auth status |
| `protocol` | string | `openai`, `openai-responses`, `anthropic`, or `google`. Required for custom slugs |
| `base_url` | string | Origin of the API. Maki appends the protocol paths |
| `plan` | string | Built-in plan key (see Plans below). Sets base URL and default model |
| `api_key_env` | string | Env var that holds the key. Defaults to `<SLUG>_API_KEY` |
| `api_key` | string | Inline key (prefer the env var or `maki auth login`) |
| `headers` | table | Extra HTTP headers sent on every request to this provider. Values expand `${VAR}` from the environment; an unset or empty variable fails the provider instead of sending a half-filled header. A same-name header (case-insensitive) replaces the built-in auth header and survives key rotation |
| `default_model` | string | Used after login when no model is saved yet |
| `discover_models` | bool | When true, also probe the provider's model list endpoint (default false) |
| `enable_free_models` | bool | Opencode only. Show free catalog models (default false) |
| `subsidised_by` | string | Name of the flat subscription prepaying this provider (e.g. `"Max"`). Models bill $0 and show the published list price beside it as a reference. The list-price fallback needs `protocol = "anthropic"` |
| `models` | array | Declared models for custom providers (see below) |
| `overrides` | table | Aperture only. Per-upstream model overrides (see below) |

### Model fields

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `id` | string | required | Model id. Spec becomes `{slug}/{id}` |
| `tier` | string | `medium` | `weak`, `medium`, `strong`, or `compaction` |
| `context_window` | u32 | protocol default | Tokens of context |
| `max_output_tokens` | u32 | protocol default | Max completion tokens |
| `supports_tool_examples` | bool | protocol default | |
| `supports_thinking` | bool | protocol default | |
| `requires_thinking` | bool | false | For APIs that reject requests with thinking disabled. Implies `supports_thinking` and raises thinking to minimal effort when off (including compaction). On generic `openai` entries without `thinking_fields` its only wire effect is that the `disabled` block is never sent |
| `thinking_fields` | table | unset | How this model spells each thinking mode on the wire. The only thinking control on generic `openai` entries, and it implies `supports_thinking`. A typo'd level key fails the parse (exit 2) |
| `supports_vision` | bool | protocol default | When false, image input and `view_image` are off |
| `pricing_input` / `pricing_output` | f64 | 0 | USD per 1M tokens |
| `pricing_cache_write` / `pricing_cache_read` | f64 | 0 | USD per 1M tokens |
| `pricing_fast_input` / `pricing_fast_output` | f64 | unset | Fast-mode pricing when the provider supports it |

Custom slugs must not reuse a built-in provider name. A bad TOML parse exits with code 2 at startup so a typo cannot silently empty the registry.

Custom `openai`-protocol models control thinking through declared `thinking_fields`. Each key is a thinking mode, and its JSON fragment merges into the request body. A model without `thinking_fields` sends `"thinking": {"type": "disabled"}` when thinking is off and nothing when it is on, the same request as before `thinking_fields` existed. GLM and Kimi gateways read that block to stop reasoning. Declaring `thinking_fields` replaces it, so add an `off` key if your gateway needs one. Effort levels snap to the declared ones, downwards first and up to the lowest key when they sit below all of them. `off` and `adaptive` need explicit keys and never snap:

```toml
[[my-ollama.models]]
id = "qwen3.8-coder-27b-mlx:latest"
supports_thinking = true

[my-ollama.models.thinking_fields]
off = { reasoning_effort = "none" }
adaptive = { reasoning_effort = "medium" }
low = { reasoning_effort = "low" }
medium = { reasoning_effort = "medium" }
high = { reasoning_effort = "xhigh" }
max = { reasoning_effort = "xhigh" }
```

A mode you left out sends nothing. To get Ollama's own effort words (`low`, `medium`, `high`, and `none` when thinking is off) instead of writing every fragment yourself, use the built-in `ollama` slug: set `[ollama].base_url` (or `OLLAMA_HOST`) and give `[[ollama.models]]` the thinking keys. Only `supports_thinking`, `requires_thinking` and `thinking_fields` overlay onto a built-in slug. The rest of the entry stays ignored, and startup names the keys it dropped.

You can also create a custom provider interactively with `maki auth login` and choosing the custom option. That writes a starter entry to this file.

### Aperture overrides

Aperture proxies upstream providers, exposing each model as `aperture/<upstream>/<model>`. Overrides keyed by upstream provider id live under `[aperture.overrides]`:

```toml
[aperture.overrides.llmserver]
base = "llama-cpp"
context_window = 131072
max_output_tokens = 16384

[aperture.overrides.llmserver.models."qwen-3.6"]
context_window = 262144
supports_vision = true
```

Provider-level fields apply to every model from that upstream; per-model entries under `models` win field by field. Fields: `context_window`, `max_output_tokens`, `supports_thinking`, `supports_vision`, `base` (remaps an opaque vendor to a native provider; e.g. `llama-cpp`, `google`, `anthropic`), and `path_prefix`. Model ids containing dots must be quoted (`"qwen3.6"`) since TOML treats a bare dotted key as a nested table.

Maki sends `/v1` (or `/v1beta` for Gemini routes, nothing for Anthropic and Z.AI), and Aperture appends that path to the upstream's base url. If an upstream base url already carries its own path, set `path_prefix = ""` for it to avoid a doubled path. Z.AI defaults to no prefix since its API path has no `/v1` segment; point the upstream base url at the full API root (e.g. `https://api.z.ai/api/paas/v4`).

### Plans

Some built-ins ship multiple plans (different base URLs or default models). `maki auth login <provider>` asks which plan to use when more than one exists. You can also set it in TOML:

```toml
[mistral]
plan = "coding"

[zai]
plan = "coding"
```

Current plans:

| Provider | Plan | What it does |
|----------|------|--------------|
| Mistral | `standard` | Standard at `https://api.mistral.ai/v1`, default `mistral/mistral-medium-latest` |
| Mistral | `coding` | Vibe / Coding at `https://api.mistral.ai/v1`, default `mistral/mistral-vibe-cli-latest` |
| Z.AI | `standard` | Pay-as-you-go at `https://api.z.ai/api/paas/v4`, default `zai/glm-5.1` |
| Z.AI | `coding` | Coding plan at `https://api.z.ai/api/coding/paas/v4`, default `zai/glm-5-code` |

Env `<SLUG>_BASE_URL` still wins over both the plan and a `base_url` in this file.

## Plugin Providers

A Lua plugin can add a provider without any change to Maki. Call `maki.provider.register` while the plugin loads, and the models it declares become addressable as `{slug}/{model_id}` (e.g. `acme/acme-large`). They show up in `/model` and in the picker, and their requests go through the same retry, pricing and usage accounting as a built-in provider.

```lua
maki.provider.register({
  slug = "acme",
  display_name = "Acme",
  codec = "openai",
  base_url = "https://api.acme.com/v1",
  api_key_env = "ACME_API_KEY",
  models = {
    { prefixes = { "acme-large" }, tier = "strong", context_window = 200000 },
  },
})
```

The plugin needs the `net` permission and must name the hosts it talks to in its `plugin.toml`. Declaring `api_key_env` also needs `env`, since resolving it reads your environment and the key saved for the slug:

```toml
[permissions]
net = true
env = true
net_hosts = ["api.acme.com"]
```

That list is what Maki sends this provider's credentials to. Maki checks it against both the plugin's own `maki.net` calls and the `base_url` the provider ends up using, so a hook cannot repoint a token at a host the manifest never declared. A `base_url` must also be `https`, or `http` pointing at loopback: a declared host reached in cleartext still puts the token on the wire. Registering with an empty or absent list fails at load.

The origin you chose yourself is the exception. If `<SLUG>_BASE_URL` or `providers.toml` points the slug at a gateway, the provider's hooks reach that gateway too, since its requests already go there.

See [plugin permissions](/docs/lua-api/#plugin-permissions) for the pattern language and how approval works.

The full field reference lives in the [Lua API](/docs/lua-api/#maki-provider). This page covers what the choices mean for the provider you are building.

### codec or base

A registration sets exactly one of `codec` and `base`. Setting both, or neither, fails at load with the plugin named.

`codec` is the supported surface for a third-party provider. Pick the wire format the API speaks:

| `codec` | Wire format |
|---------|-------------|
| `openai` | OpenAI chat completions |
| `openai-responses` | OpenAI responses API |
| `anthropic` | Anthropic messages |
| `google` | Gemini `generateContent` |

`base` names a native provider and borrows that provider's whole adapter, quirks included: Ollama's handling of the thinking field, Copilot's endpoint routing. It exists for people moving an old provider script over, where `base` was the only way to describe a provider. A new provider is better off with a codec, because a base can change behaviour whenever the provider it names does. The list of bases is fixed, and removing one is a deliberate breaking change. Valid values: `anthropic`, `openai`, `google`, `copilot`, `ollama`, `llama-cpp`, `zai`, `opencode`, `xai`, `aperture`.

Either choice also supplies defaults. A registration with no `models` table borrows the catalog of its codec or base.

### The models table

`models` is static data, read once at registration, so it must not depend on anything the plugin asks at runtime. For a catalog that is only known at runtime, use `list_models` instead.

Each row carries `prefixes`, a list. The row answers for every model id that starts with one of its prefixes, and the longest matching prefix wins, so `acme-large-2504` reads an `acme-large` row rather than an `acme` one. `prefixes[1]` is the canonical id, used wherever Maki has to name one concrete model: the picker, tier defaults, and `{slug}/{model_id}` specs.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `prefixes` | list of strings | required | Every id this row answers for. The first is the canonical id |
| `tier` | string | `medium` | `weak`, `medium`, `strong`, or `compaction` |
| `context_window` | number | 128000 | Tokens of context |
| `max_output_tokens` | number | 16384 | Max completion tokens |
| `supports_thinking` | bool | unset | |
| `requires_thinking` | bool | `false` | For APIs that reject a request with thinking off. Implies `supports_thinking` and raises thinking to minimal effort when off |
| `supports_vision` | bool | unset | When false, image input and `view_image` are off for this model |
| `supports_tool_examples` | bool | unset | |
| `pricing` | table | unset | `input`, `output`, `cache_write`, `cache_read`, in dollars per 1M tokens |
| `thinking_fields` | table | unset | How this model spells each thinking mode on the wire |

`supports_thinking`, `supports_vision` and `supports_tool_examples` have three states. Leaving one out is different from setting it to `false`: omitted asks the codec or base provider, `false` turns the feature off for that model. Declare them only for ids where you know the answer.

`thinking_fields` works as in [providers.toml](#providers-toml). Keys are `off`, `adaptive`, and the effort levels `minimal`, `low`, `medium`, `high`, `xhigh`, `max`. Each value is a JSON fragment merged into the request body, nesting included. A mode you leave out sends nothing on a codec, and falls back to the base provider's own mapping with `base = "llama-cpp"` or `base = "ollama"`.

### Callbacks

Every callback is optional. A registration with none is a static provider that reads its key from `api_key_env`.

| Field | Signature | When it runs |
|-------|-----------|--------------|
| `resolve_auth` | `function(purpose)` | Once, lazily, before the first request |
| `refresh_auth` | `function(purpose)` | After a 401, before one silent retry |
| `reload_auth` | `function(purpose)` | When Maki re-reads what a login wrote |
| `list_models` | `function()` | Model listing: the picker, `maki models` |
| `build_body` | `function(body, model, opts)` | Every request, on the final body |
| `map_error` | `function(status, message)` | On an API error, before it reaches the UI |
| `fetch_usage` | `function()` | Quota and usage display |
| `login` | `function(ctx)` | `maki auth login <slug>` |
| `logout` | `function(ctx)` | `maki auth logout <slug>` |

The three auth entries are one hook with three purposes. Whichever entries you write serve the rest, so a plugin that reads its credentials fresh every time can write `resolve_auth` alone and get refresh and reload for free. They return `{ base_url = ..., headers = { ... } }`, and omitting `base_url` keeps the one already in force.

An auth hook may write the store as well as read it. Maki holds the cross-process lock on that provider's credentials while `resolve` and `refresh` run, and a `maki.provider.auth.set` inside one re-enters that lock instead of waiting on it, so a `refresh_auth` that rotates a token can persist what it minted.

Credentials resolve lazily, on the first request that needs them. A plugin provider whose credentials are missing or expired stays in the picker and fails when you send a message, the same as a built-in provider with an unset API key. Provider scripts behaved the other way round: a `resolve` that failed took the provider out of the list, so a stale token looked like a missing provider.

Writing a `login` function is what makes the slug an auth target. There is no `has_auth` flag: a provider with `login` appears in `maki auth login`, one without it is an API-key provider and says so when asked to log in. The `ctx` handed to `login` and `logout` speaks to the terminal with `ctx.print(text)`, `ctx.prompt({ label = ..., secret = true })` and `ctx.open_url(url)`. Call them with a dot, since `ctx` is a plain table of functions.

`map_error` returns `{ status = ..., message = ... }`, or nil to keep the error as it was. Those two fields are all it can change. It cannot set `retry_after`, which is what the server asked for in the response header, and it cannot decide whether an error is retryable, which Maki derives from the status. Use it to turn an opaque vendor body into a message a person can act on.

An option the target cannot honour fails at registration rather than turning into a no-op at request time. `build_body` is accepted only with `codec = "openai"` or `codec = "openai-responses"`, and `system_prefix` is rejected with `codec = "google"`, because the Gemini path drops it. Either one fails naming the slug, the option and the target.

### Credentials

`maki.provider.auth` is a credential store Maki owns and the plugin fills:

```lua
maki.provider.auth.set("acme", { access_token = token, expires_at = when })
local creds = maki.provider.auth.get("acme")
maki.provider.auth.clear("acme")
```

The value is a free-form JSON object. Maki decides where it lives and who can read it, the plugin decides what is in it. Each slug is one file at `~/.local/state/maki/auth/plugins/<slug>.json`, apart from every credential Maki keeps for anything else, created with mode 0600, replaced in a single atomic step, and locked against other Maki processes touching the same provider. A plugin can only reach slugs it registered itself.

### Slug rules

- Must start with a letter or digit
- Only letters, digits, underscores, and hyphens after that
- Cannot reuse a slug defined in `providers.toml`
- Cannot be a built-in provider's slug. A declaration inherits that provider's `api_key_env`, so the key you set for the built-in would be handed to the plugin and sent to whatever hosts its `net` permission names. Only the provider plugins Maki ships inside the binary may claim a built-in slug, and they inherit the built-in's display name, `api_key_env`, curated model table and pricing. Restating any of those is a registration error, because the built-in's row stays the one source for them
- Two plugins cannot declare the same slug
- Registration only works while plugins load, so it belongs at the top level of the plugin file

### Migrating from provider scripts

Earlier versions loaded executable scripts from the config `providers/` directory and talked to them over stdout JSON. That mechanism is gone. Every part of it has a Lua equivalent, and the port is mechanical:

| Script subcommand | Lua |
|-------------------|-----|
| `info` returning `display_name`, `base`, `system_prefix`, `has_auth` | The same fields on the registration table. `has_auth` is gone, because a `login` function is what makes the provider an auth target |
| `models` returning model rows | The static `models` table. `id` becomes `prefixes`, a list, which is what the field always was: `id = "acme"` becomes `prefixes = { "acme" }` and matches the same ids |
| `resolve` | `resolve_auth` |
| `refresh` | `refresh_auth` |
| `reload` | `reload_auth` |
| `login` over inherited stdio | `login = function(ctx)`, using `ctx.print`, `ctx.prompt` and `ctx.open_url` |
| `logout` over inherited stdio | `logout = function(ctx)` |
| No equivalent | `build_body`, `map_error`, `fetch_usage`, `list_models` |

A script that stored its credentials in its own file can keep them. Import that file the first time `resolve_auth` runs and hand it to Maki:

```lua
local function credentials()
  local stored = maki.provider.auth.get("acme")
  if stored then
    return stored
  end
  local text = maki.fs.read(maki.fs.normalize("~/.maki/auth/acme.json"))
  if not text then
    return nil
  end
  local imported = maki.json.decode(text)
  maki.provider.auth.set("acme", imported)
  return imported
end
```

Nobody has to log in again. The first request after the upgrade reads the old file once and writes it into Maki's store, and every later run reads it from there.

### Worked example

The plugin below is Maki's test fixture for the provider API, quoted from the file the test suite runs, so it cannot drift from the API it documents. It speaks `codec = "openai"` and uses every hook once.

Its manifest:

```toml
[permissions]
net = true
env = true
net_hosts = ["api.acme.example", "127.0.0.1"]
```

The plugin itself:

```lua
-- An OpenAI-compatible provider written entirely in Lua.
--
-- Every hook `maki.provider.register` accepts appears once, with the reason it
-- exists, so this file doubles as the worked example in the plugin docs.

local SLUG = "acmelua"
local ANONYMOUS = "anonymous"
local BASE_URL = maki.uv.os_getenv("ACME_BASE_URL") or "https://api.acme.example/v1"

-- The token `login` stored, or none at all.
local function stored_token()
  local stored = maki.provider.auth.get(SLUG)
  return (stored and stored.token) or ANONYMOUS
end

local function lease(token)
  return { base_url = BASE_URL, headers = { authorization = "Bearer " .. token } }
end

maki.provider.register({
  slug = SLUG,
  display_name = "Acme (Lua)",
  codec = "openai",
  base_url = BASE_URL,
  -- Prepended to whatever system prompt maki assembled, so house rules the
  -- provider needs ride along without the agent having to know about them.
  system_prefix = "Acme house rules: answer in full sentences.",
  models = {
    {
      prefixes = { "acme-1", "acme" },
      tier = "strong",
      context_window = 200000,
      max_output_tokens = 8192,
      supports_thinking = true,
      -- The only two levels Acme accepts; maki snaps anything else onto them.
      thinking_fields = {
        low = { reasoning_effort = "low" },
        high = { reasoning_effort = "high" },
      },
    },
  },

  -- Called once, lazily, before the first request of the session.
  resolve_auth = function()
    return lease(stored_token())
  end,

  -- Called after a 401 that arrived before any output. An Acme lease is single
  -- use, so the stored credential buys the next one instead of being resent,
  -- and the new one is stored right here: maki holds this provider's credential
  -- lock while the hook runs and lets the hook itself back in through it.
  refresh_auth = function()
    local renewed = stored_token() .. "-renewed"
    maki.provider.auth.set(SLUG, { token = renewed })
    return lease(renewed)
  end,

  -- Called when the store changed underneath us, e.g. after `maki auth login`
  -- ran in another process.
  reload_auth = function()
    return lease(stored_token())
  end,

  -- Acme's catalogue moves faster than this file, so the picker asks the API.
  list_models = function()
    return { { id = "acme-1", context_window = 200000, tier = "strong" } }
  end,

  -- Runs on the final body, after maki rendered the thinking level into it, so
  -- what arrives here is exactly what goes on the wire. Acme wants the effort
  -- under its own key and rejects OpenAI's.
  build_body = function(body, model, opts)
    body.acme_reasoning = { model = model, effort = body.reasoning_effort, asked_for = opts.thinking }
    body.reasoning_effort = nil
    return body
  end,

  -- Acme answers 429 for a spent monthly allowance, which no retry can fix.
  map_error = function(status, message)
    if status == 429 and message:find("allowance") then
      return { status = 400, message = "Acme allowance is spent until the next cycle" }
    end
  end,

  fetch_usage = function()
    return { plan = "team", limits = { { label = "Monthly allowance", percentage = 42 } } }
  end,

  -- Having a `login` is what makes this provider an auth target: it shows up in
  -- `maki auth login` because this function exists.
  login = function(ctx)
    local key = ctx.prompt({ label = "Acme API key: ", secret = true })
    if not key or key == "" then
      ctx.print("No key entered, nothing was stored.")
      return
    end
    maki.provider.auth.set(SLUG, { token = key })
    ctx.print("Stored your Acme key.")
  end,

  logout = function(ctx)
    maki.provider.auth.clear(SLUG)
    ctx.print("Forgot your Acme key.")
  end,
})
```

