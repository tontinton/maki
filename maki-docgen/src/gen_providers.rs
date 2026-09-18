use maki_providers::Effort;
use maki_providers::model::{ModelEntry, ModelTier};
use maki_providers::spec::{AuthDoc, CatalogDoc, ProviderRegistry, ProviderSpec};
use std::fmt::Write;

const FRONT_MATTER: &str = r#"+++
title = "Providers"
weight = 5
[extra]
group = "Reference"
+++"#;

const TIER_PICKER_NOTE: &str = r#"Open the model picker with `/model` and press `!`, `@`, `#`, or `$` on any row to assign it to strong, medium, weak, or compaction. Press the same key again to remove the assignment. Your overrides are saved to `~/.local/state/maki/model-tiers` and apply across sessions."#;

const AUTH_RELOADING: &str = r#"## Auth Reloading

Maki re-reads auth from storage and environment variables each time a new agent spawns (`/new`, retry, session load). If you run `maki auth login` in another terminal or change an env var, the next session picks it up without a restart.

You can set multiple API keys in one env var (`ANTHROPIC_API_KEY=sk-1,sk-2,sk-3`). On a rate-limit or auth error, maki switches to the next key right away, with no delay and without spending a retry. Each request walks the pool once before falling back to normal backoff. A plan quota error does not rotate keys, since the quota is per account and the others are just as spent."#;

const BASE_URL_OVERRIDES: &str = r#"## Base URL Overrides

Every provider honors a `<SLUG>_BASE_URL` env var (`anthropic` -> `ANTHROPIC_BASE_URL`, `llama-cpp` -> `LLAMA_CPP_BASE_URL`). Set it to the origin of a proxy or a compatible endpoint and Maki appends the API paths itself:

```sh
ANTHROPIC_BASE_URL=https://my-proxy.internal maki
```

It wins over `providers.toml` and built-in defaults. `ANTHROPIC_BASE_URL` and `OPENAI_BASE_URL` are the same names the official SDKs use, so an existing proxy setup carries over as is. Two exceptions: `OPENAI_BASE_URL` only redirects the platform API, never the ChatGPT Coding Plan backend; `XAI_BASE_URL` only redirects the public API-key endpoint, never the OAuth CLI proxy.

You can also set `base_url` for a built-in provider in `~/.config/maki/providers.toml`. It overrides the built-in default and loses to the env var above:

```toml
[openai]
base_url = "http://xxxx:1234/v1"
```

The built-in provider still owns the slug, so `protocol`, `api_key_env`, `discover_models` and `models` are ignored with a warning. Use a custom slug if you need those."#;

const MODEL_IDENTIFIERS: &str = r#"## Model Identifiers

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

New models start at the **medium** tier until you assign one in the picker."#;

fn providers_toml_section() -> String {
    let mut plan_rows = String::new();
    let mut plan_examples = String::new();
    let mut builtins: Vec<_> = maki_config::providers::all_builtins();
    builtins.sort_by_key(|b| b.slug);
    let mut wrote_example = false;
    for b in builtins {
        let Some(plans) = b.plans.filter(|p| p.len() > 1) else {
            continue;
        };
        if !wrote_example {
            let _ = writeln!(plan_examples, "```toml");
            wrote_example = true;
        } else {
            let _ = writeln!(plan_examples);
        }
        // Prefer a non-default plan key in the example when one exists.
        let example_key = plans
            .iter()
            .find(|(_, p)| {
                p.base_url != b.default_base_url || p.default_model != Some(b.default_model)
            })
            .unwrap_or(&plans[0])
            .0;
        let _ = writeln!(plan_examples, "[{}]", b.slug);
        let _ = writeln!(plan_examples, "plan = \"{example_key}\"");
        for (key, plan) in plans {
            let mut detail = plan.display_name.to_string();
            if !plan.base_url.is_empty() {
                detail = format!("{detail} at `{}`", plan.base_url);
            }
            if let Some(model) = plan.default_model {
                detail = format!("{detail}, default `{model}`");
            }
            let _ = writeln!(plan_rows, "| {} | `{key}` | {detail} |", b.display_name);
        }
    }
    if wrote_example {
        let _ = writeln!(plan_examples, "```");
    }

    let plans_body = if plan_rows.is_empty() {
        "No built-in currently ships more than one plan.".to_string()
    } else {
        format!(
            "Some built-ins ship multiple plans (different base URLs or default models). \
`maki auth login <provider>` asks which plan to use when more than one exists. \
You can also set it in TOML:\n\n\
{plan_examples}\n\
Current plans:\n\n\
| Provider | Plan | What it does |\n\
|----------|------|--------------|\n\
{plan_rows}\n\
Env `<SLUG>_BASE_URL` still wins over both the plan and a `base_url` in this file."
        )
    };

    format!(
        r#"## providers.toml

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
| `headers` | table | Extra HTTP headers sent on every request to this provider. Values expand `${{VAR}}` from the environment; an unset or empty variable fails the provider instead of sending a half-filled header. A same-name header (case-insensitive) replaces the built-in auth header and survives key rotation |
| `default_model` | string | Used after login when no model is saved yet |
| `discover_models` | bool | When true, also probe the provider's model list endpoint (default false) |
| `enable_free_models` | bool | Opencode only. Show free catalog models (default false) |
| `subsidised_by` | string | Name of the flat subscription prepaying this provider (e.g. `"Max"`). Models bill $0 and show the published list price beside it as a reference. The list-price fallback needs `protocol = "anthropic"` |
| `models` | array | Declared models for custom providers (see below) |
| `overrides` | table | Aperture only. Per-upstream model overrides (see below) |

### Model fields

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `id` | string | required | Model id. Spec becomes `{{slug}}/{{id}}` |
| `tier` | string | `medium` | `weak`, `medium`, `strong`, or `compaction` |
| `context_window` | u32 | protocol default | Tokens of context |
| `max_output_tokens` | u32 | protocol default | Max completion tokens |
| `supports_tool_examples` | bool | protocol default | |
| `supports_thinking` | bool | protocol default | |
| `requires_thinking` | bool | false | For APIs that reject requests with thinking disabled. Implies `supports_thinking` and raises thinking to minimal effort when off (including compaction). On generic `openai` entries without `thinking_fields` it has no wire effect |
| `thinking_fields` | table | unset | How this model spells each thinking mode on the wire. The only thinking control on generic `openai` entries, and it implies `supports_thinking`. A typo'd level key fails the parse (exit 2) |
| `supports_vision` | bool | protocol default | When false, image input and `view_image` are off |
| `pricing_input` / `pricing_output` | f64 | 0 | USD per 1M tokens |
| `pricing_cache_write` / `pricing_cache_read` | f64 | 0 | USD per 1M tokens |
| `pricing_fast_input` / `pricing_fast_output` | f64 | unset | Fast-mode pricing when the provider supports it |

Custom slugs must not reuse a built-in provider name. A bad TOML parse exits with code 2 at startup so a typo cannot silently empty the registry.

Custom `openai`-protocol models send thinking only through declared `thinking_fields`. Each key is a thinking mode, and its JSON fragment merges into the request body. A model without `thinking_fields` sends no thinking at all, so a plain gateway keeps receiving the request it received before. Effort levels snap to the declared ones, downwards first and up to the lowest key when they sit below all of them. `off` and `adaptive` need explicit keys and never snap:

```toml
[[my-ollama.models]]
id = "qwen3.8-coder-27b-mlx:latest"
supports_thinking = true

[my-ollama.models.thinking_fields]
off = {{ reasoning_effort = "none" }}
adaptive = {{ reasoning_effort = "medium" }}
low = {{ reasoning_effort = "low" }}
medium = {{ reasoning_effort = "medium" }}
high = {{ reasoning_effort = "xhigh" }}
max = {{ reasoning_effort = "xhigh" }}
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

{plans_body}"#
    )
}

fn dynamic_providers_section() -> String {
    let valid_values: Vec<String> = ProviderRegistry::native_slugs()
        .map(|slug| format!("`{slug}`"))
        .collect();
    let efforts: Vec<String> = Effort::ALL.iter().map(|e| format!("`{e}`")).collect();

    format!(
        r#"## Dynamic Providers

To add a custom provider or proxy, drop an executable script into the config `providers/` directory (`~/.config/maki/providers/` on Linux/macOS, `%APPDATA%\maki\providers\` on Windows). The script must handle these subcommands:

| Subcommand | Timeout | What it does |
|------------|---------|--------|
| `info` | 5s | Return JSON with `display_name`, `base` provider, `has_auth` |
| `models` | 5s | Return JSON array of model entries (optional) |
| `resolve` | 30s | Return auth JSON (`base_url`, `headers`) |
| `login` | interactive | OAuth or credential flow |
| `logout` | interactive | Clear credentials |
| `refresh` | 30s | Refresh auth tokens |

`resolve` is called each time a new agent spawns, so scripts should read tokens from disk instead of caching them in memory. That way auth changes from other processes get picked up.

The `base` field specifies which built-in provider to inherit the model catalog from. Valid values: {}.

If your provider serves models not in the base catalog, add a `models` subcommand returning:

```json
[{{"id": "my-model-v2", "tier": "strong", "context_window": 200000, "max_output_tokens": 16384}}]
```

Only `id` is required. Optional fields: `tier` (default `medium`), `context_window` (128K), `max_output_tokens` (16K), `pricing` (`{{input, output, cache_write, cache_read}}`, all per 1M tokens), `supports_tool_examples` (defaults to the base provider's setting), `supports_thinking` (defaults to the base provider's setting), `requires_thinking` (default false; for APIs that reject requests with thinking off, raises it to minimal effort and implies `supports_thinking`), `supports_vision` (defaults to the base provider's setting; when false, image input and the `view_image` tool are disabled). The first model listed per tier is used for sub-agents. Without this subcommand, the base provider's models are used.

A `llama-cpp`, `ollama`, or `openai` base model can replace Maki's token-budget mapping with its native thinking fields. Each thinking mode maps to a JSON fragment merged into the request body:

```json
[{{
  "id": "reasoning-model",
  "supports_thinking": true,
  "thinking_fields": {{
    "off": {{"reasoning_effort": "none"}},
    "adaptive": {{"reasoning_effort": "medium"}},
    "low": {{"reasoning_effort": "low"}},
    "medium": {{"reasoning_effort": "medium"}},
    "xhigh": {{"reasoning_effort": "xhigh"}}
  }}
}}]
```

`off` is used when thinking is off, `adaptive` when thinking is on without a chosen level. Any other key is an effort level, one of {}. The levels you declare are the ones the model accepts: whatever you ask for snaps into them, downwards first, so a level the model never advertised is never sent. Every part is optional, but `off` and `adaptive` never snap: a mode you left undeclared sends nothing on the generic `openai` path, and falls back to the base provider's mapping on `llama-cpp` and `ollama`.

Fragments are merged into the body, so nesting works too. A template toggle is just a fragment:

```json
"thinking_fields": {{
  "off": {{"chat_template_kwargs": {{"enable_thinking": false}}}},
  "adaptive": {{"chat_template_kwargs": {{"enable_thinking": true}}}}
}}
```

Named modes send only these fields, no token budget. An explicit `/thinking <budget>` snaps into the levels you declared; a model that declares none gets the `adaptive` fragment plus `thinking_budget_tokens`. Any other undeclared effort level falls back to the usual `thinking_budget_tokens` mapping, so no request ever ends up saying nothing. Models without `thinking_fields` keep the base provider's behavior.

Dynamic provider models are namespaced as `{{slug}}/{{model_id}}` (e.g. `myproxy/claude-sonnet-4-6`).

### Script Name Rules

- Must start with a letter or digit
- Only letters, digits, underscores, and hyphens after that
- Can't reuse a built-in provider's slug
- Must be executable"#,
        valid_values.join(", "),
        efforts.join(", "),
    )
}

fn tier_label(tier: ModelTier) -> &'static str {
    match tier {
        ModelTier::Weak => "Weak",
        ModelTier::Medium => "Medium",
        ModelTier::Strong => "Strong",
        ModelTier::Compaction => "Compaction",
    }
}

fn format_pricing(entry: &ModelEntry) -> String {
    format!("${:.2} / ${:.2}", entry.pricing.input, entry.pricing.output)
}

fn format_context(entry: &ModelEntry) -> String {
    let ctx_k = entry.context_window / 1_000;
    match entry.max_output_tokens {
        Some(out) => format!("{ctx_k}K ctx / {}K out", out / 1_000),
        None => format!("{ctx_k}K ctx"),
    }
}

fn write_model_table(out: &mut String, entries: &[ModelEntry]) {
    let _ = writeln!(
        out,
        "| Tier | Models | Pricing (in/out per 1M tokens) | Context |"
    );
    let _ = writeln!(
        out,
        "|------|--------|-------------------------------|---------|"
    );

    // A row per model, not per tier: prices and context sizes differ inside a
    // tier, so one merged row would quote a single model's numbers for all.
    for tier in [ModelTier::Weak, ModelTier::Medium, ModelTier::Strong] {
        for entry in entries.iter().filter(|e| e.tier == tier) {
            let names = entry.prefixes.join(", ");
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                tier_label(tier),
                if entry.default {
                    format!("**{names}** (default)")
                } else {
                    names
                },
                format_pricing(entry),
                format_context(entry),
            );
        }
    }

    let defaults: Vec<String> = entries
        .iter()
        .filter(|e| e.default)
        .map(|e| {
            format!(
                "{} ({})",
                e.prefixes.first().unwrap_or(&"?"),
                tier_label(e.tier).to_lowercase(),
            )
        })
        .collect();

    if !defaults.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Defaults: {}", defaults.join(", "));
    }
}

fn write_section(out: &mut String, spec: &ProviderSpec) {
    let docs = &spec.docs;
    let _ = writeln!(out, "### {}\n", spec.display_name);
    let auth_line = match docs.auth {
        AuthDoc::EnvVar => format!("`{}`", spec.api_key_env),
        AuthDoc::EnvVarWith(note) => format!("`{}` {note}", spec.api_key_env),
        AuthDoc::Custom(line) => line.to_string(),
    };
    let _ = writeln!(out, "- **Env var**: {auth_line}");

    if let [url] = docs.api_urls {
        let _ = writeln!(out, "- **API**: `{url}`");
    } else {
        let _ = writeln!(out, "- **API endpoints**:");
        for url in docs.api_urls {
            let _ = writeln!(out, "  - `{url}`");
        }
    }

    if let Some(features) = docs.features {
        let _ = writeln!(out, "- **Features**: {features}");
    }

    // Rendered from the schedule, so the docs cannot drift from what we bill.
    if let Some(schedule) = spec.pricing_schedule {
        let _ = writeln!(
            out,
            "- **Peak pricing**: the prices below are off-peak; each turn is billed as it happens, at {schedule}"
        );
    }

    let _ = writeln!(out);

    match docs.catalog {
        CatalogDoc::Table => write_model_table(out, spec.models),
        CatalogDoc::Discovered(note) => {
            let _ = writeln!(out, "{note}");
        }
    }

    for note in docs.trailing_notes {
        let _ = writeln!(out, "\n{note}");
    }
}

pub fn generate() -> String {
    let mut out = String::with_capacity(4096);

    let _ = writeln!(out, "{FRONT_MATTER}\n");
    let _ = writeln!(out, "# Providers\n");
    let _ = writeln!(
        out,
        "Maki talks to LLM providers over their HTTP APIs. \
         Models are split into three tiers: **weak** (cheap and fast), \
         **medium** (balanced), and **strong** (highest capability, highest cost). \
         There is also a **compaction** tier for choosing a dedicated model to summarize context when the conversation grows long.\n"
    );
    let _ = writeln!(out, "{TIER_PICKER_NOTE}\n");
    let _ = writeln!(out, "{AUTH_RELOADING}\n");
    let _ = writeln!(out, "{BASE_URL_OVERRIDES}\n");
    let _ = writeln!(out, "## Built-in Providers\n");

    // `BUILTINS` order is the documentation order, stated on the array.
    for spec in ProviderRegistry::builtins() {
        write_section(&mut out, spec);
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "{MODEL_IDENTIFIERS}\n");
    let _ = writeln!(out, "{}\n", providers_toml_section());
    let _ = writeln!(out, "{}", dynamic_providers_section());

    out
}
