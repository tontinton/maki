use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use serde::Deserialize;

use crate::model::{ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::spec::{NO_CURATED_MODELS, ProviderRegistry};

const SLUG_MISMATCH: &str = "slug header";
const NO_PREFIXES: &str = "no prefixes";
const DUPLICATE_PREFIX: &str = "duplicate prefix";
const DUPLICATE_DEFAULT: &str = "second default for tier";
const OUTPUT_EXCEEDS_WINDOW: &str = "exceeds context_window";

/// The file format of `models/<slug>.toml`. `slug` is a checksum rather than
/// data: nothing reads it except [`parse`], which is how an `include_str!`
/// pointing at the wrong table gets caught.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelTable {
    slug: String,
    model: Vec<CuratedRow>,
}

/// One curated row as the file spells it. Stricter than the plugin surface
/// [`ModelEntry`] decodes: `max_output_tokens` is the only field allowed a
/// serde default, because TOML has no null and an absent limit really does
/// mean "the provider never published one". Everything else missing, or
/// spelled wrong, is a mistake worth hearing about.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CuratedRow {
    prefixes: Vec<String>,
    tier: ModelTier,
    family: ModelFamily,
    vision: bool,
    default: bool,
    pricing: ModelPricing,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    context_window: u32,
}

impl From<CuratedRow> for ModelEntry {
    fn from(row: CuratedRow) -> Self {
        Self {
            prefixes: row.prefixes,
            tier: row.tier,
            family: Some(row.family),
            supports_vision: Some(row.vision),
            default: row.default,
            pricing: Some(row.pricing),
            max_output_tokens: row.max_output_tokens,
            context_window: Some(row.context_window),
            ..Self::default()
        }
    }
}

/// Every curated table, parsed once and keyed by the slug that owns it. Keyed
/// by slug rather than by spec identity because a `ProviderSpec` is a `const`
/// that call sites copy, so there is no single address to key on.
static TABLES: LazyLock<HashMap<&'static str, Box<[ModelEntry]>>> = LazyLock::new(|| {
    ProviderRegistry::builtins()
        .iter()
        .map(|spec| {
            // Const fields only. Calling `ProviderSpec::models()` here would
            // re-enter this `LazyLock` and hang.
            let table = match spec.models_toml {
                NO_CURATED_MODELS => Box::default(),
                src => parse(spec.slug, src).unwrap_or_else(|e| panic!("{e}")),
            };
            (spec.slug, table)
        })
        .collect()
});

/// Empty for any slug that is not a builtin, which is the right answer: those
/// providers hand out their models from a live catalog.
pub(crate) fn table(slug: &str) -> &'static [ModelEntry] {
    TABLES.get(slug).map_or(&[], |table| table)
}

/// The input is embedded at compile time, so the only caller turns an error
/// into a panic. `spec::tests::every_builtin_model_table_parses` is what keeps
/// a broken table off a user's machine.
fn parse(slug: &str, src: &str) -> Result<Box<[ModelEntry]>, String> {
    let file = format!("models/{slug}.toml");
    let table: ModelTable = toml::from_str(src).map_err(|e| format!("{file}: {e}"))?;
    if table.slug != slug {
        return Err(format!(
            "{file}: {SLUG_MISMATCH} {:?} does not match {slug:?}",
            table.slug
        ));
    }

    let mut prefixes_seen: HashSet<&str> = HashSet::new();
    let mut defaults_seen: Vec<ModelTier> = Vec::new();

    for (index, row) in table.model.iter().enumerate() {
        let Some(name) = row.prefixes.first() else {
            return Err(format!("{file}: row {}: {NO_PREFIXES}", index + 1));
        };
        let at = format!("{file} {name:?}");

        for prefix in &row.prefixes {
            if !prefixes_seen.insert(prefix) {
                return Err(format!("{at}: {DUPLICATE_PREFIX} {prefix:?}"));
            }
        }
        if let Some(max_output) = row.max_output_tokens
            && max_output > row.context_window
        {
            return Err(format!(
                "{at}: max_output_tokens {max_output} {OUTPUT_EXCEEDS_WINDOW} {}",
                row.context_window
            ));
        }
        if row.default {
            if defaults_seen.contains(&row.tier) {
                return Err(format!("{at}: {DUPLICATE_DEFAULT} {:?}", row.tier));
            }
            defaults_seen.push(row.tier);
        }
    }

    Ok(table.model.into_iter().map(ModelEntry::from).collect())
}

#[cfg(test)]
mod tests {
    use super::{
        DUPLICATE_DEFAULT, DUPLICATE_PREFIX, NO_PREFIXES, OUTPUT_EXCEEDS_WINDOW, SLUG_MISMATCH,
        parse,
    };
    use test_case::test_case;

    const SLUG: &str = "synthetic";
    const ACCEPTED: &str = "table should have been rejected";
    const UNKNOWN_KEY: &str = "unknown field";
    const MISSING_KEY: &str = "missing field";

    const ROW: &str = r#"
        [[model]]
        prefixes = ["a"]
        tier = "strong"
        family = "generic"
        vision = false
        default = true
        context_window = 200000
        pricing = { input = 1.0, output = 2.0, cache_write = 0.0, cache_read = 0.0 }
    "#;

    fn table(slug: &str, rows: &str) -> String {
        format!("slug = \"{slug}\"\n{rows}")
    }

    #[test_case(&table("regolo", ROW), SLUG_MISMATCH ; "slug_header_names_another_provider")]
    #[test_case(&table(SLUG, &ROW.replace(r#"["a"]"#, "[]")), NO_PREFIXES ; "row_without_prefixes")]
    #[test_case(&format!("{}{}", table(SLUG, ROW), ROW.replace("default = true", "default = false")), DUPLICATE_PREFIX ; "prefix_repeated_in_table")]
    #[test_case(&format!("{}{}", table(SLUG, ROW), ROW.replace(r#"["a"]"#, r#"["b"]"#)), DUPLICATE_DEFAULT ; "two_defaults_for_one_tier")]
    #[test_case(&table(SLUG, &format!("{ROW}max_output_tokens = 200001\n")), OUTPUT_EXCEEDS_WINDOW ; "output_exceeds_window")]
    #[test_case(&table(SLUG, &ROW.replace("vision", "visoin")), UNKNOWN_KEY ; "unknown_key")]
    #[test_case(&table(SLUG, &ROW.replace("vision = false", "")), MISSING_KEY ; "missing_key")]
    fn parse_rejects(src: &str, expected: &str) {
        let message = parse(SLUG, src).expect_err(ACCEPTED);
        assert!(
            message.contains(expected),
            "expected {expected:?} in {message:?}"
        );
    }

    #[test]
    fn optionals_are_none_until_a_row_declares_them() {
        const MAX_OUTPUT: u32 = 64_000;
        const FAST_INPUT: f64 = 10.0;
        const FAST_OUTPUT: f64 = 50.0;
        const PARSED: &str = "table should have parsed";

        let bare = parse(SLUG, &table(SLUG, ROW)).expect(PARSED);
        assert_eq!(bare[0].max_output_tokens, None);
        assert!(bare[0].pricing.as_ref().expect(PARSED).fast.is_none());

        let declared = table(SLUG, ROW).replace(
            "cache_read = 0.0 }",
            &format!(
                "cache_read = 0.0, fast = {{ input = {FAST_INPUT}, output = {FAST_OUTPUT} }} }}\nmax_output_tokens = {MAX_OUTPUT}"
            ),
        );
        let entries = parse(SLUG, &declared).expect(PARSED);
        let fast = entries[0]
            .pricing
            .as_ref()
            .and_then(|pricing| pricing.fast.as_ref())
            .expect(PARSED);

        assert_eq!(entries[0].max_output_tokens, Some(MAX_OUTPUT));
        assert_eq!(fast.input, FAST_INPUT);
        assert_eq!(fast.output, FAST_OUTPUT);
    }
}
