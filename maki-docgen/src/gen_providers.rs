use maki_providers::model::{ModelEntry, ModelSet, ModelTier, models_for_provider};
use maki_providers::provider::ProviderKind;
use std::fmt::Write;
use strum::IntoEnumIterator;

const FRONT_MATTER: &str = r#"+++
title = "Providers"
weight = 5
[extra]
group = "Reference"
+++"#;

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
| `info` | 5s | Return JSON with `display_name`, `base` provider, optional `models`, and `has_auth` |
| `resolve` | 30s | Return auth JSON (`base_url`, `headers`) |
| `login` | interactive | OAuth or credential flow |
| `logout` | interactive | Clear credentials |
| `refresh` | 30s | Refresh auth tokens |

`resolve` is called each time a new agent spawns, so scripts should read tokens from disk instead of caching them in memory. That way auth changes from other processes get picked up.

The `base` field specifies which built-in provider to inherit defaults from. Valid values: {}. For example, a proxy in front of Anthropic sets `base` to `anthropic`, so Claude model ids and defaults stay available while requests route through your auth and base URL.

If `models` is omitted, the dynamic provider inherits the base provider's model catalog. `openai-coding-plan` is the exception and must declare an explicit models list.

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
    auth: AuthDoc,
    endpoint: EndpointDoc,
    features: Option<&'static str>,
    notes: Option<Vec<&'static str>>,
    entries: &'static [ModelEntry],
}

enum AuthDoc {
    ApiKey {
        env_var: &'static str,
        note: Option<&'static str>,
    },
    OAuthDevice {
        command: &'static str,
        note: Option<&'static str>,
    },
}

enum EndpointDoc {
    Api(&'static str),
    ApiList(Vec<&'static str>),
    Transport(String),
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
                    auth: AuthDoc::ApiKey {
                        env_var: ProviderKind::Zai.api_key_env().unwrap(),
                        note: Some("shared across both endpoints"),
                    },
                    endpoint: EndpointDoc::ApiList(vec![
                        ProviderKind::Zai.base_url(),
                        ProviderKind::ZaiCodingPlan.base_url(),
                    ]),
                    features: ProviderKind::Zai.features(),
                    notes: None,
                    entries: models_for_provider(ProviderKind::Zai, ModelSet::All),
                });
            }
            ProviderKind::ZaiCodingPlan => {
                zai_done = true;
            }
            ProviderKind::OpenAi => {
                sections.push(ProviderSection {
                    name: kind.display_name(),
                    auth: AuthDoc::ApiKey {
                        env_var: kind.api_key_env().unwrap(),
                        note: None,
                    },
                    endpoint: EndpointDoc::Api(kind.base_url()),
                    features: kind.features(),
                    notes: None,
                    entries: models_for_provider(kind, ModelSet::All),
                });
            }
            ProviderKind::OpenAiCodingPlan => {
                sections.push(ProviderSection {
                    name: kind.display_name(),
                    auth: AuthDoc::OAuthDevice {
                        command: "maki auth login openai",
                        note: None,
                    },
                    endpoint: EndpointDoc::Transport(format!("{}/responses", kind.base_url())),
                    features: kind.features(),
                    notes: Some(vec![
                        "Uses ChatGPT subscription plans, not the OpenAI Platform API.",
                    ]),
                    entries: models_for_provider(kind, ModelSet::Visible),
                });
            }
            _ => {
                sections.push(ProviderSection {
                    name: kind.display_name(),
                    auth: AuthDoc::ApiKey {
                        env_var: kind.api_key_env().unwrap(),
                        note: None,
                    },
                    endpoint: EndpointDoc::Api(kind.base_url()),
                    features: kind.features(),
                    notes: None,
                    entries: models_for_provider(kind, ModelSet::All),
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
    match &section.auth {
        AuthDoc::ApiKey { env_var, note } => {
            if let Some(note) = note {
                let _ = writeln!(out, "- **Env var**: `{env_var}` ({note})");
            } else {
                let _ = writeln!(out, "- **Env var**: `{env_var}`");
            }
        }
        AuthDoc::OAuthDevice { command, note } => {
            if let Some(note) = note {
                let _ = writeln!(
                    out,
                    "- **Auth**: OAuth device flow via `{command}` ({note})"
                );
            } else {
                let _ = writeln!(out, "- **Auth**: OAuth device flow via `{command}`");
            }
        }
    }

    match &section.endpoint {
        EndpointDoc::Api(url) => {
            let _ = writeln!(out, "- **API**: `{url}`");
        }
        EndpointDoc::Transport(url) => {
            let _ = writeln!(out, "- **Transport**: `{url}`");
        }
        EndpointDoc::ApiList(urls) => {
            let _ = writeln!(out, "- **API endpoints**:");
            for url in urls {
                let _ = writeln!(out, "  - `{url}`");
            }
        }
    }

    if let Some(features) = section.features {
        let _ = writeln!(out, "- **Features**: {features}");
    }
    if let Some(notes) = &section.notes {
        for note in notes {
            let _ = writeln!(out, "- **Notes**: {note}");
        }
    }

    let _ = writeln!(out);
    write_model_table(out, section.entries);
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
