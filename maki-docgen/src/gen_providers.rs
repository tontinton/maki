use maki_providers::model::{ModelEntry, ModelTier, models_for_provider};
use maki_providers::provider::ProviderKind;
use std::fmt::Write;
use strum::IntoEnumIterator;

const FRONT_MATTER: &str = r#"+++
title = "Providers"
weight = 5
[extra]
group = "Reference"
+++"#;

const TIER_PICKER_NOTE: &str = r#"Open the model picker with `/model` and press `1`, `2`, or `3` on any row to reassign it to strong, medium, or weak. Your overrides are saved to `~/.maki/model-tiers` and apply across sessions."#;

const AUTH_RELOADING: &str = r#"## Auth Reloading

Maki re-reads auth from storage and environment variables each time a new agent spawns (`/new`, retry, session load). If you run `maki auth login` in another terminal or change an env var, the next session picks it up without a restart."#;

const MODEL_IDENTIFIERS: &str = r#"## Model Identifiers

Models are referenced as `provider/model_id`:

```
anthropic/claude-sonnet-4-6
openai/gpt-4.1
zai/glm-4.7
```

If the model name is unique across providers, the prefix can be omitted."#;

fn dynamic_providers_section() -> String {
    let valid_values: Vec<String> = ProviderKind::iter().map(|k| format!("`{k}`")).collect();

    format!(
        r#"## Dynamic Providers

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

The `base` field specifies which built-in provider to inherit the model catalog from. Valid values: {}.

If your provider serves models not in the base catalog, add a `models` subcommand returning:

```json
[{{"id": "my-model-v2", "tier": "strong", "context_window": 200000, "max_output_tokens": 16384}}]
```

Only `id` is required. Optional fields: `tier` (default `medium`), `context_window` (128K), `max_output_tokens` (16K), `pricing` (`{{input, output, cache_write, cache_read}}`, all per 1M tokens), `supports_tool_examples` (defaults to the base provider's setting). The first model listed per tier is used for sub-agents. Without this subcommand, the base provider's models are used.

Dynamic provider models are namespaced as `{{slug}}/{{model_id}}` (e.g. `myproxy/claude-sonnet-4-6`).

### Script Name Rules

- Must start with a letter or digit
- Only letters, digits, underscores, and hyphens after that
- Can't reuse a built-in provider's slug
- Must be executable"#,
        valid_values.join(", "),
    )
}

fn tier_label(tier: ModelTier) -> &'static str {
    match tier {
        ModelTier::Weak => "Weak",
        ModelTier::Medium => "Medium",
        ModelTier::Strong => "Strong",
    }
}

fn format_pricing(entry: &ModelEntry) -> String {
    format!("${:.2} / ${:.2}", entry.pricing.input, entry.pricing.output)
}

fn format_context(entry: &ModelEntry) -> String {
    let ctx_k = entry.context_window / 1_000;
    let out_k = entry.max_output_tokens / 1_000;
    format!("{ctx_k}K ctx / {out_k}K out")
}

struct ProviderSection {
    name: &'static str,
    auth_line: String,
    urls: Vec<&'static str>,
    features: Option<&'static str>,
    entries: &'static [ModelEntry],
}

fn format_auth(kind: ProviderKind) -> String {
    let env = kind.api_key_env();
    if kind == ProviderKind::Ollama {
        format!("`{env}` for cloud, or `OLLAMA_HOST` for local (e.g. `http://localhost:11434`)")
    } else {
        format!("`{env}`")
    }
}

fn build_sections() -> Vec<ProviderSection> {
    let mut sections = Vec::new();
    let mut zai_done = false;

    for kind in ProviderKind::iter() {
        match kind {
            ProviderKind::Zai => {
                if zai_done {
                    continue;
                }
                zai_done = true;
                sections.push(ProviderSection {
                    name: "Z.AI",
                    auth_line: format!(
                        "{} (shared across both endpoints)",
                        format_auth(ProviderKind::Zai)
                    ),
                    urls: vec![
                        ProviderKind::Zai.base_url(),
                        ProviderKind::ZaiCodingPlan.base_url(),
                    ],
                    features: ProviderKind::Zai.features(),
                    entries: models_for_provider(ProviderKind::Zai),
                });
            }
            ProviderKind::ZaiCodingPlan => {
                zai_done = true;
            }
            ProviderKind::OpenAi => {
                sections.push(ProviderSection {
                    name: kind.display_name(),
                    auth_line: format!("{} (also supports OAuth device flow)", format_auth(kind)),
                    urls: vec![kind.base_url()],
                    features: kind.features(),
                    entries: models_for_provider(kind),
                });
            }
            _ => {
                sections.push(ProviderSection {
                    name: kind.display_name(),
                    auth_line: format_auth(kind),
                    urls: vec![kind.base_url()],
                    features: kind.features(),
                    entries: models_for_provider(kind),
                });
            }
        }
    }

    sections
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

    for tier in [ModelTier::Weak, ModelTier::Medium, ModelTier::Strong] {
        let tier_entries: Vec<_> = entries.iter().filter(|e| e.tier == tier).collect();
        if tier_entries.is_empty() {
            continue;
        }

        let models: Vec<String> = tier_entries
            .iter()
            .map(|e| {
                let names = e.prefixes.join(", ");
                if e.default {
                    format!("**{names}** (default)")
                } else {
                    names
                }
            })
            .collect();

        let pricing = tier_entries
            .first()
            .map(|e| format_pricing(e))
            .unwrap_or_default();
        let context = tier_entries
            .first()
            .map(|e| format_context(e))
            .unwrap_or_default();

        let _ = writeln!(
            out,
            "| {} | {} | {} | {} |",
            tier_label(tier),
            models.join(", "),
            pricing,
            context,
        );
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

fn write_section(out: &mut String, section: &ProviderSection) {
    let _ = writeln!(out, "### {}\n", section.name);
    let _ = writeln!(out, "- **Env var**: {}", section.auth_line);

    if section.urls.len() == 1 {
        let _ = writeln!(out, "- **API**: `{}`", section.urls[0]);
    } else {
        let _ = writeln!(out, "- **API endpoints**:");
        for url in &section.urls {
            let _ = writeln!(out, "  - `{url}`");
        }
    }

    if let Some(features) = section.features {
        let _ = writeln!(out, "- **Features**: {features}");
    }

    let _ = writeln!(out);

    if section.entries.is_empty() {
        let _ = writeln!(
            out,
            "Maki asks your local Ollama for the list of installed models, so there's no built-in catalog. Tiers are guessed from list order: the first model becomes strong, the second medium, and the rest weak. If that guess is wrong, open `/model` and press `1`, `2`, or `3` on any row to reassign it. Your choices are saved to `~/.maki/model-tiers`."
        );
    } else {
        write_model_table(out, section.entries);
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
         **medium** (balanced), and **strong** (highest capability, highest cost).\n"
    );
    let _ = writeln!(out, "{TIER_PICKER_NOTE}\n");
    let _ = writeln!(out, "{AUTH_RELOADING}\n");
    let _ = writeln!(out, "## Built-in Providers\n");

    for section in &build_sections() {
        write_section(&mut out, section);
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "{MODEL_IDENTIFIERS}\n");
    let _ = writeln!(out, "{}", dynamic_providers_section());

    out
}
