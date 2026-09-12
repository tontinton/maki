use std::fmt::Display;
use std::io::{self, Write};
use std::sync::{Mutex, MutexGuard, PoisonError};

use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};

use maki_config::SessionDefaults;
use maki_providers::manifest::ManifestRegistry;
use maki_providers::model::{Model, ModelError, ModelTier};
use maki_storage::StateDir;
use maki_storage::log::RotatingFileWriter;
use maki_storage::model::{read_model, read_thinking};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;

const LOG_ENV: &str = "MAKI_LOG";
const LOG_ENV_SHARED: &str = "RUST_LOG";
const DEFAULT_LOG_LEVEL: LevelFilter = LevelFilter::INFO;

const PROVIDER_PRIORITY: &[&str] = &[
    "anthropic",
    "openai",
    "xai",
    "copilot",
    "zai",
    "synthetic",
    "deepseek",
    "regolo",
];

pub fn resolve_model(
    explicit: Option<&str>,
    provider_config: &maki_config::ProviderConfig,
    storage: &StateDir,
) -> Result<Model> {
    let policy = &provider_config.model_policy;
    if let Some(spec) = explicit {
        if !policy.allows(spec) {
            return Err(eyre!(
                "model {spec:?} is not allowed by provider model policy"
            ));
        }
        return from_spec_or_warm_catalog(spec).context("invalid --model spec");
    }
    if let Some(spec) = read_model(storage) {
        if policy.allows(&spec)
            && let Ok(m) = from_spec_or_warm_catalog(&spec)
        {
            return Ok(m);
        }
        tracing::warn!(
            spec,
            "saved model unavailable or disallowed, falling back to default"
        );
    }
    if let Some(spec) = provider_config.default_model.as_deref() {
        if !policy.allows(spec) {
            return Err(eyre!(
                "default model {spec:?} is not allowed by provider model policy"
            ));
        }
        return from_spec_or_warm_catalog(spec).context("invalid default_model in config");
    }
    auto_detect_model(policy).ok_or_else(|| {
        let policy_note = if policy.is_restrictive() {
            "\nnote: an allowed_models/excluded_models policy is active and may exclude every candidate"
        } else {
            ""
        };
        color_eyre::eyre::eyre!(
            "no provider available - set an API key (e.g. ANTHROPIC_API_KEY), run `maki auth login`, or use -m to specify a model{policy_note}\n\nSee https://maki.sh/docs/providers/ for setup instructions"
        )
    })
}

/// `always_thinking` pins the level. Without it, a fresh session starts at
/// whatever `/thinking` last set, the way the model comes back too; a model
/// that cannot run that level clamps it when the session loads.
pub fn remember_thinking(defaults: &mut SessionDefaults, storage: &StateDir) {
    if defaults.thinking.is_none() {
        defaults.thinking = read_thinking(storage);
    }
}

/// An unknown slug may just mean the models.dev catalog has not been loaded
/// yet, so retry once with a warm catalog. `Model::from_spec` itself must stay
/// non-blocking: the UI draws with it.
fn from_spec_or_warm_catalog(spec: &str) -> Result<Model, ModelError> {
    match Model::from_spec(spec) {
        Err(ModelError::UnsupportedProvider(_)) => {
            maki_providers::warm_catalog();
            Model::from_spec(spec)
        }
        result => result,
    }
}

fn auto_detect_model(policy: &maki_config::ModelPolicy) -> Option<Model> {
    for tier in [ModelTier::Strong, ModelTier::Medium] {
        for &slug in PROVIDER_PRIORITY {
            if maki_providers::provider::provider_available(slug)
                && let Ok(model) = Model::from_tier(slug, tier)
                && policy.allows(&model.spec())
            {
                return Some(model);
            }
        }
    }
    None
}

/// Built-in slugs keep their compiled protocol, model catalog and auth wiring,
/// so a `providers.toml` entry setting those fields is only partly honored
/// (#597). Call this after `init_logging`, otherwise the warning has no
/// subscriber to reach.
pub fn warn_ignored_provider_fields() {
    for (slug, def) in &maki_config::providers::ProvidersConfig::load().providers {
        if ManifestRegistry::get(slug).is_none() {
            continue;
        }
        let ignored = maki_config::providers::ignored_builtin_fields(slug, def);
        if ignored.is_empty() {
            continue;
        }
        tracing::warn!(
            slug,
            fields = %ignored.join(", "),
            "providers.toml entry for built-in provider ignores these fields \
             (base_url/plan/api_key still apply), use a custom slug to set \
             protocol or models"
        );
    }
}

pub fn install_panic_log_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic payload".into()
        };
        let location = info.location().map(|l| l.to_string());
        tracing::error!(
            panic.payload = %payload,
            panic.location = location.as_deref().unwrap_or("<unknown>"),
            "panic occurred"
        );
        prev(info);
    }));
}

/// Telemetry is opt-in and must never stop maki from starting, so a bad
/// setting is a warning in the log, not an error to the user. Call this after
/// `init_logging` or the warning has nowhere to go.
pub fn init_telemetry(config: &maki_config::TelemetryConfig) {
    if let Err(error) = maki_otel::init(config) {
        tracing::warn!(%error, "telemetry disabled");
    }
}

/// Headless runs without a session id still count, they just stay
/// unattributed.
pub fn report_session_start(start_type: &'static str, session_id: Option<impl Display>) {
    let id = session_id.map(|id| id.to_string());
    maki_otel::emit::session_started(start_type, id.as_deref());
}

/// The writer every event goes through.
///
/// A bare `Mutex` would do, except `tracing_subscriber`'s blanket impl unwraps
/// the poison, so a single panic raised while a line was being written turned
/// every later log call into a panic of its own. Taking the guard back out of
/// the poison keeps a wounded process writing its own post mortem.
struct SharedWriter(Mutex<RotatingFileWriter>);

struct SharedGuard<'a>(MutexGuard<'a, RotatingFileWriter>);

impl Write for SharedGuard<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<'a> MakeWriter<'a> for SharedWriter {
    type Writer = SharedGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        SharedGuard(self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// `MAKI_LOG` is ours and is taken literally, `MAKI_LOG=off` included.
///
/// `RUST_LOG` belongs to every Rust program on the machine, and `EnvFilter`
/// drops any target no directive names, so a `RUST_LOG=other_crate=debug` left
/// in a shell profile for an unrelated project silenced maki entirely. We still
/// read it, and a value that names a level decides ours, but a value that only
/// lists other people's targets gets our level added underneath, so a foreign
/// variable can add to our logs and never take them away.
fn log_filter() -> EnvFilter {
    let read = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    build_log_filter(read(LOG_ENV).as_deref(), read(LOG_ENV_SHARED).as_deref())
}

/// Pure core of `log_filter`, so tests never have to set a process wide
/// variable and race every other test in the binary.
fn build_log_filter(ours: Option<&str>, shared: Option<&str>) -> EnvFilter {
    for (name, value, is_ours) in [(LOG_ENV, ours, true), (LOG_ENV_SHARED, shared, false)] {
        let Some(value) = value else {
            continue;
        };
        match EnvFilter::builder().parse(value) {
            Ok(filter) if is_ours || sets_a_level(value) => return filter,
            Ok(filter) => return filter.add_directive(DEFAULT_LOG_LEVEL.into()),
            Err(error) => eprintln!("maki: ignoring {name}={value:?}: {error}"),
        }
    }
    EnvFilter::default().add_directive(DEFAULT_LOG_LEVEL.into())
}

/// Whether a filter string carries a bare level such as `debug`, as opposed to
/// only per target directives like `foo=debug`.
///
/// Empty pieces are skipped here, as `EnvFilter` skips them too. `LevelFilter`
/// reads an empty string as `error`, so the trailing comma in
/// `RUST_LOG=other_crate=debug,` would look like a level and cost us the log.
fn sets_a_level(value: &str) -> bool {
    value
        .split(',')
        .map(str::trim)
        .any(|part| !part.is_empty() && part.parse::<LevelFilter>().is_ok())
}

/// Logging is the first thing to set up and the last thing allowed to stop the
/// program, but a failure here used to be swallowed whole, so an unwritable log
/// dir looked exactly like an idle one: no logs, no reason, no clue.
pub fn init_logging(storage_config: &maki_config::StorageConfig) {
    let writer =
        match RotatingFileWriter::new(storage_config.max_log_bytes, storage_config.max_log_files) {
            Ok(writer) => writer,
            Err(error) => {
                eprintln!("maki: logging disabled, cannot open the log file: {error}");
                return;
            }
        };
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(log_filter())
        .with_writer(SharedWriter(Mutex::new(writer)))
        .init();
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{DEFAULT_LOG_LEVEL, build_log_filter};

    const FOREIGN: &str = "other_crate=debug";
    const FOREIGN_EMPTY_PIECES: &str = ",other_crate=debug,,";
    const BROKEN: &str = "not a directive=!!";

    #[test_case(None, None, "info" ; "nothing_set_logs_at_info")]
    #[test_case(Some("warn"), None, "warn" ; "our_variable_wins")]
    #[test_case(Some("off"), None, "off" ; "our_variable_can_silence_us")]
    #[test_case(Some("warn"), Some("trace"), "warn" ; "our_variable_beats_the_shared_one")]
    #[test_case(None, Some("debug"), "debug" ; "shared_variable_with_a_level_is_taken_as_is")]
    #[test_case(None, Some(FOREIGN), "info" ; "foreign_directive_cannot_silence_us")]
    #[test_case(None, Some(FOREIGN_EMPTY_PIECES), "info" ; "empty_pieces_are_not_a_level")]
    #[test_case(None, Some(BROKEN), "info" ; "unparsable_value_falls_back")]
    fn log_filter_keeps_our_own_logs_alive(
        ours: Option<&str>,
        shared: Option<&str>,
        expected: &str,
    ) {
        let rendered = build_log_filter(ours, shared).to_string();
        assert!(
            rendered.split(',').any(|d| d == expected),
            "{ours:?} + {shared:?} rendered as {rendered:?}, wanted {expected:?}"
        );
    }

    #[test]
    fn foreign_directive_is_kept_alongside_our_level() {
        let rendered = build_log_filter(None, Some(FOREIGN)).to_string();
        assert!(rendered.contains(FOREIGN), "{FOREIGN} was dropped");
        assert!(rendered.contains(&DEFAULT_LOG_LEVEL.to_string()));
    }
}
