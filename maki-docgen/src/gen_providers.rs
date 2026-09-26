use maki_providers::Effort;
use maki_providers::model::{ModelEntry, ModelTier};
use maki_providers::spec::{AuthDoc, BASES, CatalogDoc, ProviderRegistry, ProviderSpec};
use std::fmt::Write;

const FRONT_MATTER: &str = r#"+++
title = "Providers"
weight = 5
[extra]
group = "Reference"
+++"#;

/// The provider fixture the `maki-lua` test suite loads, quoted verbatim so the
/// worked example cannot drift from an API that still passes its own tests.
const PLUGIN_PROVIDER_EXAMPLE: &str =
    include_str!("../../maki-lua/tests/fixtures/provider_plugin/init.lua");
const PLUGIN_PROVIDER_MANIFEST: &str =
    include_str!("../../maki-lua/tests/fixtures/provider_plugin/plugin.toml");

const TIER_PICKER_NOTE: &str = r#"Open the model picker with `/model` and press `!`, `@`, `#`, or `$` on any row to assign it to strong, medium, weak, or compaction. Press the same key again to remove the assignment. Your overrides are saved to `~/.local/state/maki/model-tiers` and apply across sessions."#;

const AUTH_RELOADING: &str = r#"## Auth Reloading

Maki re-reads auth from storage and environment variables each time a new agent spawns (`/new`, retry, session load). If you run `maki auth login` in another terminal or change an env var, the next session picks it up without a restart.

You can set multiple API keys in one env var (`ANTHROPIC_API_KEY=sk-1,sk-2,sk-3`). On a rate-limit or auth error, maki switches to the next key right away, with no delay and without spending a retry. Each request walks the pool once before falling back to normal backoff. A plan quota error does not rotate keys, since the quota is per account and the others are just as spent."#;

const BASE_URL_OVERRIDES: &str = r#"## Base URL Overrides

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
| `requires_thinking` | bool | false | For APIs that reject requests with thinking disabled. Implies `supports_thinking` and raises thinking to minimal effort when off (including compaction). On generic `openai` entries without `thinking_fields` its only wire effect is that the `disabled` block is never sent |
| `thinking_fields` | table | unset | How this model spells each thinking mode on the wire. The only thinking control on generic `openai` entries, and it implies `supports_thinking`. A typo'd level key fails the parse (exit 2) |
| `supports_vision` | bool | protocol default | When false, image input and `view_image` are off |
| `pricing_input` / `pricing_output` | f64 | 0 | USD per 1M tokens |
| `pricing_cache_write` / `pricing_cache_read` | f64 | 0 | USD per 1M tokens |
| `pricing_fast_input` / `pricing_fast_output` | f64 | unset | Fast-mode pricing when the provider supports it |

Custom slugs must not reuse a built-in provider name. A bad TOML parse exits with code 2 at startup so a typo cannot silently empty the registry.

Custom `openai`-protocol models control thinking through declared `thinking_fields`. Each key is a thinking mode, and its JSON fragment merges into the request body. A model without `thinking_fields` sends `"thinking": {{"type": "disabled"}}` when thinking is off and nothing when it is on, the same request as before `thinking_fields` existed. GLM and Kimi gateways read that block to stop reasoning. Declaring `thinking_fields` replaces it, so add an `off` key if your gateway needs one. Effort levels snap to the declared ones, downwards first and up to the lowest key when they sit below all of them. `off` and `adaptive` need explicit keys and never snap:

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

fn plugin_providers_section() -> String {
    let bases: Vec<String> = BASES.iter().map(|slug| format!("`{slug}`")).collect();
    let efforts: Vec<String> = Effort::ALL.iter().map(|e| format!("`{e}`")).collect();

    format!(
        r#"## Plugin Providers

A Lua plugin can add a provider without any change to Maki. Call `maki.provider.register` while the plugin loads, and the models it declares become addressable as `{{slug}}/{{model_id}}` (e.g. `acme/acme-large`). They show up in `/model` and in the picker, and their requests go through the same retry, pricing and usage accounting as a built-in provider.

```lua
maki.provider.register({{
  slug = "acme",
  display_name = "Acme",
  codec = "openai",
  base_url = "https://api.acme.com/v1",
  api_key_env = "ACME_API_KEY",
  models = {{
    {{ prefixes = {{ "acme-large" }}, tier = "strong", context_window = 200000 }},
  }},
}})
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

`base` names a native provider and borrows that provider's whole adapter, quirks included: Ollama's handling of the thinking field, Copilot's endpoint routing. It exists for people moving an old provider script over, where `base` was the only way to describe a provider. A new provider is better off with a codec, because a base can change behaviour whenever the provider it names does. The list of bases is fixed, and removing one is a deliberate breaking change. Valid values: {}.

Either choice also supplies defaults. A registration with no `models` table borrows the catalog of its codec or base.

### The models table

`models` is static data, read once at registration, so it must not depend on anything the plugin asks at runtime. For a catalog that is only known at runtime, use `list_models` instead.

Each row carries `prefixes`, a list. The row answers for every model id that starts with one of its prefixes, and the longest matching prefix wins, so `acme-large-2504` reads an `acme-large` row rather than an `acme` one. `prefixes[1]` is the canonical id, used wherever Maki has to name one concrete model: the picker, tier defaults, and `{{slug}}/{{model_id}}` specs.

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

`thinking_fields` works as in [providers.toml](#providers-toml). Keys are `off`, `adaptive`, and the effort levels {}. Each value is a JSON fragment merged into the request body, nesting included. A mode you leave out sends nothing on a codec, and falls back to the base provider's own mapping with `base = "llama-cpp"` or `base = "ollama"`.

### Callbacks

Every callback is optional. A registration with none is a static provider that reads its key from `api_key_env`.

| Field | Signature | When it runs |
|-------|-----------|--------------|
| `auth` | `function(ctx, purpose)` | `"resolve"` once, lazily, before the first request. `"refresh"` after a 401, before one silent retry. `"reload"` when Maki re-reads what a login wrote |
| `list_models` | `function(ctx)` | Model listing: the picker, `maki models` |
| `build_body` | `function(ctx, body, model, opts)` | Every request, on the final body |
| `map_error` | `function(ctx, status, message)` | On an API error, before it reaches the UI |
| `fetch_usage` | `function(ctx)` | Quota and usage display |
| `login` | `function(ctx)` | `maki auth login <slug>` |
| `logout` | `function(ctx)` | `maki auth logout <slug>` |

Every callback gets a `ctx` table first, read when the call starts:

| Field | What it holds |
|-------|---------------|
| `ctx.slug` | The slug the callback serves |
| `ctx.base_url` | The origin a request would reach right now: an origin `auth` returned, then `<SLUG>_BASE_URL` or `providers.toml`, then the declared `base_url` |
| `ctx.headers` | The headers every request to the slug carries, as a snapshot |
| `ctx.get_json(target)` | A GET with `ctx.headers`. A `target` starting with `/` is appended to `ctx.base_url`, an absolute url is sent as is. Returns the decoded body, or nil and an error the callback can return as its own |

`ctx.get_json` goes out under the same host rules as `maki.net.request` and never retries. An error status and a connection that failed both come back as the error a built-in provider raises for them, so `return nil, err` keeps retries and `Retry-After` working. Reading `ctx.base_url` rather than writing an origin into the plugin keeps a side call on the same gateway as the chat requests when a user points the slug somewhere else.

`auth` returns `{{ base_url = ..., headers = {{ ... }} }}`, and omitting `base_url` keeps the one already in force. A plugin that reads its credentials fresh every time can ignore `purpose`.

An auth hook may write the store as well as read it. Maki holds the cross-process lock on that provider's credentials during `resolve` and `refresh`, and a `maki.provider.auth.set` inside the hook re-enters that lock instead of waiting on it, so a refresh that rotates a token can persist what it minted.

Credentials resolve lazily, on the first request that needs them. A plugin provider whose credentials are missing or expired stays in the picker and fails when you send a message, the same as a built-in provider with an unset API key. Provider scripts behaved the other way round: a `resolve` that failed took the provider out of the list, so a stale token looked like a missing provider.

Writing a `login` function is what makes the slug an auth target. There is no `has_auth` flag: a provider with `login` appears in `maki auth login`, one without it is an API-key provider and says so when asked to log in. The `ctx` handed to `login` and `logout` also speaks to the terminal with `ctx.print(text)`, `ctx.prompt({{ label = ..., secret = true }})` and `ctx.open_url(url)`. Call them with a dot, since `ctx` is a plain table of functions.

`map_error` returns `{{ status = ..., message = ... }}`, or nil to keep the error as it was. Those two fields are all it can change. It cannot set `retry_after`, which is what the server asked for in the response header, and it cannot decide whether an error is retryable, which Maki derives from the status. Use it to turn an opaque vendor body into a message a person can act on.

An option the target cannot honour fails at registration rather than turning into a no-op at request time. `build_body` is accepted only with `codec = "openai"` or `codec = "openai-responses"`, and `system_prefix` is rejected with `codec = "google"`, because the Gemini path drops it. Either one fails naming the slug, the option and the target.

### Credentials

`maki.provider.auth` is a credential store Maki owns and the plugin fills:

```lua
maki.provider.auth.set("acme", {{ access_token = token, expires_at = when }})
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
| `models` returning model rows | The static `models` table. `id` becomes `prefixes`, a list, which is what the field always was: `id = "acme"` becomes `prefixes = {{ "acme" }}` and matches the same ids |
| `resolve`, `refresh`, `reload` | `auth = function(ctx, purpose)`, with `purpose` naming the subcommand |
| `login` over inherited stdio | `login = function(ctx)`, using `ctx.print`, `ctx.prompt` and `ctx.open_url` |
| `logout` over inherited stdio | `logout = function(ctx)` |
| No equivalent | `build_body`, `map_error`, `fetch_usage`, `list_models` |

A script that stored its credentials in its own file can keep them. Import that file the first time `auth` runs and hand it to Maki:

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
{}```

The plugin itself:

```lua
{}```
"#,
        bases.join(", "),
        efforts.join(", "),
        PLUGIN_PROVIDER_MANIFEST,
        PLUGIN_PROVIDER_EXAMPLE,
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
    entry.pricing.as_ref().map_or_else(String::new, |pricing| {
        format!("${:.2} / ${:.2}", pricing.input, pricing.output)
    })
}

fn format_context(entry: &ModelEntry) -> String {
    let Some(context_window) = entry.context_window else {
        return String::new();
    };
    let ctx_k = context_window / 1_000;
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
                e.prefixes.first().map_or("?", String::as_str),
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
        CatalogDoc::Table => write_model_table(out, spec.models()),
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
    let _ = writeln!(out, "{}", plugin_providers_section());

    out
}
