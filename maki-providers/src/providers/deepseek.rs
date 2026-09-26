use maki_config::providers::Protocol;

use crate::model::ModelFamily;
use crate::pricing::{PricingSchedule, PricingWindow};
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, Build, CatalogDoc, GeneratedDocs, LoginConfig, ProviderSpec,
};

const SLUG: &str = "deepseek";
const DISPLAY_NAME: &str = "DeepSeek";
const ENV_VAR: &str = "DEEPSEEK_API_KEY";
const BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_MODEL: &str = "deepseek/deepseek-flash";
const LOGIN_URL: &str = "https://platform.deepseek.com/api_keys";
const FEATURES: &str = "Thinking mode toggle (on/off), open-weight models";

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(384_000),
    fallback_context_window: 1_000_000,
    models_toml: include_str!("../../models/deepseek.toml"),
    pricing_schedule: Some(&PEAK_HOURS),
    build: Build::Declared,
    aperture: Some(ApertureRoute {
        path_prefix: DEFAULT_PATH_PREFIX,
    }),
    login: Some(LoginConfig {
        protocol: Protocol::Openai,
        default_base_url: BASE_URL,
        default_model: DEFAULT_MODEL,
        plans: None,
        login_url: Some(LOGIN_URL),
        needs_url: false,
    }),
    docs: GeneratedDocs {
        api_urls: &[BASE_URL],
        features: Some(FEATURES),
        auth: AuthDoc::EnvVar,
        catalog: CatalogDoc::Table,
        trailing_notes: &[],
    },
};

inventory::submit!(SPEC.config_row());

/// Peak hours double every rate, and `models/deepseek.toml` quotes the off-peak
/// ones. The weekend stays off-peak around the clock.
/// <https://api-docs.deepseek.com/quick_start/pricing/>
pub(crate) const PEAK_HOURS: PricingSchedule =
    PricingSchedule::new(PEAK_WINDOWS, PEAK_MULTIPLIER).weekdays_only();

const PEAK_WINDOWS: &[PricingWindow] = &[PricingWindow::hours(1, 4), PricingWindow::hours(6, 10)];
const PEAK_MULTIPLIER: f64 = 2.0;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::ProviderRegistry;

    /// The hours, days and surcharge as the pricing page states them.
    const PUBLISHED_PEAK_HOURS: &str = "2x during 01:00-04:00, 06:00-10:00 UTC, Mon-Fri";

    /// A schedule that never got hooked up to the spec looks exactly like
    /// off-peak all day, so the assert goes through the registry the biller
    /// reads. Drift on either side bills every DeepSeek turn at the wrong rate.
    #[test]
    fn the_manifest_bills_the_published_peak_hours() {
        let schedule = ProviderRegistry::get(SLUG)
            .expect("deepseek is a builtin")
            .pricing_schedule
            .expect("deepseek bills by the clock");
        assert_eq!(schedule.to_string(), PUBLISHED_PEAK_HOURS);
    }
}
