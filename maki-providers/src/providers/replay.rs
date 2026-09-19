//! Golden replay: one recorded exchange, one provider, one artifact on disk.
//!
//! Four things about a provider are worth comparing and this compares all
//! four: the request bytes it put on the wire, the [`ProviderEvent`]s it
//! emitted in order, the error it failed with, and the usage it came back
//! with. Each case lands in a golden file, so the suite keeps its teeth once
//! the implementation it was first written against is deleted. A differential
//! test dies with either of its two sides, a golden does not.
//!
//! Every entry point takes the step that stages the declaration to replay,
//! because only `maki-lua` can boot the plugin host that registers a bundled
//! provider plugin.
//!
//! Every golden was recorded once, against the bespoke impl the declaration
//! replaced, and is never regenerated after that impl is gone: the files are
//! the spec the plugin answers to.
//!
//! Regenerate with `UPDATE_GOLDENS=1 cargo nextest run -p maki-lua`.
//! A *missing* golden always fails, because a suite that records whatever it
//! sees on its first run has asserted nothing.

use std::collections::BTreeMap;
use std::path::PathBuf;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use maki_config::providers::base_url_env_var;
use maki_storage::id::SessionRef;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::model::{Model, ModelInfo};
use crate::model_registry;
use crate::provider::Provider;
use crate::spec::ProviderRegistry;
use crate::test_support::{Canned, Recorded, Requests, is_routed, serve};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, ThinkingConfig};

use super::{Timeouts, plugin};

const GOLDEN_DIR: &str = "tests/goldens";
const UPDATE_ENV: &str = "UPDATE_GOLDENS";
const UPDATE_ON: &str = "1";
const HOST_HEADER: &str = "host";
const USER_AGENT_HEADER: &str = "user-agent";
const VOLATILE_VALUE: &str = "<volatile>";
const REQUESTS_KEY: &str = "requests";
const BODY_KEY: &str = "body";
const PATH_KEY: &str = "path";
const EVENTS_KEY: &str = "events";
const OUTCOME_KEY: &str = "outcome";
const MODELS_KEY: &str = "models";
const DISCOVERY_KEY: &str = "discovery";
const USAGE_KEY: &str = "usage";
const RESET_AT_KEY: &str = "reset_at";
const LIMITS_POINTER: &str = "/outcome/usage/limits";
const TODAY: &str = "<today>";
const NEXT_UTC_MIDNIGHT: &str = "<next-utc-midnight>";

const PROMPT: &str = "read a.txt";
const SYSTEM: &str = "You are a replay fixture.";
const TOOL_NAME: &str = "read";
const TOOL_DESCRIPTION: &str = "Read a file";

const API_KEY: &str = "sk-replay";
const HOME_VARS: &[&str] = &[
    "HOME",
    "XDG_STATE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
];

const UNAUTHORIZED_BODY: &str = r#"{"error":{"message":"invalid api key"}}"#;
const RATE_LIMITED_BODY: &str = r#"{"error":{"message":"too many requests"}}"#;
const SERVER_ERROR_BODY: &str = r#"{"error":{"message":"internal error"}}"#;
const RETRY_AFTER_HEADERS: &[(&str, &str)] =
    &[("content-type", "application/json"), ("retry-after", "7")];

const NO_GOLDEN_DIR: &str = "the golden directory has no parent";
const WRITE_FAILED: &str = "the golden could not be recorded";
const BAD_GOLDEN: &str = "the golden on disk is not json";
const NOT_AN_OBJECT: &str = "an observation is always a json object";
const NO_REQUESTS: &str = "every observation records the requests it sent";
const TEMPDIR_FAILED: &str = "no temporary state directory";
const NOT_A_BUILTIN: &str = "a replayed slug is a builtin";
const CREATE_FAILED: &str = "the provider could not be built";
const BAD_SESSION: &str = "a fixture's session is a stored session id";
const NO_MIDNIGHT: &str = "the day after a sampled one is representable";

/// One replayed exchange: what the server answers with, and what the request
/// asks for beyond the fixed prompt.
pub struct Fixture {
    pub name: &'static str,
    /// An upper bound on the requests a run may send, not a promise that it
    /// sends them all: a provider that retries internally draws a second
    /// entry, and one that does not leaves it unserved.
    pub script: &'static [Canned],
    pub thinking: ThinkingConfig,
    /// The session the turn belongs to, as a stored id. `None` sends none,
    /// which is what every fixture that is not about session affinity wants.
    pub session: Option<&'static str>,
}

// The failures below look the same whichever provider hits them, so they are
// written once here and every port replays the ones it answers. Each provider
// still gets its own golden, since the name of a fixture is the name of its
// file inside the provider's directory.

/// A rejected key. The second answer is one neither side is expected to ask
/// for: a provider that replays the rejected key is recorded as a second
/// request instead of parking on an `accept` that never returns.
pub const UNAUTHORIZED: Fixture = Fixture {
    name: "unauthorized",
    script: &[
        Canned::json(401, UNAUTHORIZED_BODY),
        Canned::json(401, UNAUTHORIZED_BODY),
    ],
    thinking: ThinkingConfig::Off,
    session: None,
};

pub const RATE_LIMITED: Fixture = Fixture {
    name: "rate_limited",
    script: &[Canned::json(429, RATE_LIMITED_BODY)],
    thinking: ThinkingConfig::Off,
    session: None,
};

/// The same 429 with the header that tells us how long to wait, which is the
/// one thing downstream backoff reads off a rate limit.
pub const SLOW_DOWN: Fixture = Fixture {
    name: "rate_limited_with_retry_after",
    script: &[Canned {
        status: 429,
        headers: RETRY_AFTER_HEADERS,
        body: RATE_LIMITED_BODY,
        path: None,
    }],
    thinking: ThinkingConfig::Off,
    session: None,
};

pub const SERVER_ERROR: Fixture = Fixture {
    name: "server_error",
    script: &[Canned::json(500, SERVER_ERROR_BODY)],
    thinking: ThinkingConfig::Off,
    session: None,
};

/// One unparseable frame between two good ones: the bad frame is skipped and
/// the turn still ends, rather than the whole stream failing.
pub const MALFORMED_SSE: Fixture = Fixture {
    name: "malformed_sse",
    script: &[Canned::sse(
        r#"data: {"choices": [ this is not json

data: {"choices":[{"delta":{"content":"Hello"}}]}

data: [DONE]

"#,
    )],
    thinking: ThinkingConfig::Off,
    session: None,
};

/// An error frame on a 200, carrying a tag but no message. The substituted
/// message is a real bug fix (`EMPTY_SSE_ERROR_MESSAGE`): without it the turn
/// ended with an empty assistant message and no retry.
pub const EMPTY_SSE_ERROR: Fixture = Fixture {
    name: "empty_sse_error_frame",
    script: &[Canned::sse(
        r#"data: {"error":{"type":"server_error","message":""}}

"#,
    )],
    thinking: ThinkingConfig::Off,
    session: None,
};

/// Ends mid-frame, with no `finish_reason` and no `[DONE]`.
pub const TRUNCATED_STREAM: Fixture = Fixture {
    name: "truncated_stream",
    script: &[Canned::sse(
        r#"data: {"choices":[{"delta":{"content":"Hel"}}]}

data: {"choices":[{"delta":{"con"#,
    )],
    thinking: ThinkingConfig::Off,
    session: None,
};

/// The question every fixture asks, so two providers driven by this harness
/// are never answering different ones.
pub fn tools() -> Value {
    json!([{
        "name": TOOL_NAME,
        "description": TOOL_DESCRIPTION,
        "input_schema": {
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        },
    }])
}

fn turn() -> Vec<Message> {
    vec![Message::user(PROMPT.to_owned())]
}

/// Replays `fixture` through the declaration `stage` left serving `slug` and
/// pins everything that came back.
pub fn declared<T>(stage: impl FnOnce() -> T, slug: &str, fixture: &Fixture, model: &Model) {
    declared_with(stage, slug, fixture, model, &turn(), &tools());
}

/// [`declared`] for a provider whose body work reads the history or the tool
/// list. What it does to an assistant turn is invisible against the lone user
/// message [`declared`] sends, and what it does only when tools are present is
/// invisible when they always are.
pub fn declared_with<T>(
    stage: impl FnOnce() -> T,
    slug: &str,
    fixture: &Fixture,
    model: &Model,
    messages: &[Message],
    tools: &Value,
) {
    Replay::declared(stage, slug, fixture).stream(model, messages, tools);
}

/// The same recorded exchange for the other endpoint a provider answers on.
/// Kept apart from [`declared`] rather than folded into the `Fixture`: a usage
/// call sends no messages, emits no events and has no thinking mode, so a
/// shared entry point would carry three fields it never reads.
///
/// The one endpoint that reads the wall clock, so its observation is
/// normalised against it, see [`normalise_clock`].
pub fn declared_usage<T>(stage: impl FnOnce() -> T, slug: &str, fixture: &Fixture) {
    Replay::declared(stage, slug, fixture).usage();
}

/// The catalogue `slug` lists, or the failure it lists instead. Each row is
/// recorded whole, so a [`ModelInfo`] field added later lands in
/// every golden rather than slipping past a hand-picked projection.
pub fn declared_models<T>(stage: impl FnOnce() -> T, slug: &str, fixture: &Fixture) {
    Replay::declared(stage, slug, fixture).models();
}

/// A turn whose request depends on what discovery found: `list_models`, its
/// rows handed to the registry the way startup does, then the turn, all drawn
/// from one script. A failed listing is recorded and the turn still runs,
/// undiscovered.
pub fn declared_discovered<T>(
    stage: impl FnOnce() -> T,
    slug: &str,
    fixture: &Fixture,
    model: &Model,
) {
    Replay::declared(stage, slug, fixture).discovered(model);
}

/// One exchange ready to run: the provider under test, what the recorded
/// server has seen, and the world both of them live in.
///
/// The world is a guard: the temporary tree is read while the request is
/// built, and a hook whose plugin host has died answers nothing. Fields drop
/// in order, so the provider goes before the host it talks to, and the host
/// before the home it may write to on its way out.
struct Replay<'a, G> {
    provider: Box<dyn Provider>,
    requests: Requests,
    slug: &'a str,
    fixture: &'a Fixture,
    _world: (G, TempDir),
}

impl<'a, G> Replay<'a, G> {
    /// `stage` registers inside the load window the way a plugin load does,
    /// and `create` resolves the inherited `api_key_env` into a key pool right
    /// away, so the claim on the built-in slug is exercised instead of
    /// assumed.
    fn declared(stage: impl FnOnce() -> G, slug: &'a str, fixture: &'a Fixture) -> Self {
        Self::open(slug, fixture, || {
            plugin::begin_load();
            let staged = stage();
            plugin::commit_load();
            let provider = plugin::create(slug, Timeouts::default()).expect(CREATE_FAILED);
            (provider, staged)
        })
    }

    /// Stands up the whole world one exchange needs: a throwaway home with
    /// the key the slug reads and the recorded server, then `build`s `slug`
    /// inside it.
    ///
    /// Every base directory moves, so no run touches this machine's
    /// credentials, `providers.toml` or saved origins. The registry, the
    /// environment and the credential store are all process-global, and
    /// `cargo nextest` gives each test its own process, which is what keeps
    /// one fixture's key and origin out of the next one's.
    ///
    /// Loopback is published through `<SLUG>_BASE_URL` because that is the
    /// only rung of the precedence a test can reach. The declaration's own
    /// `base_url` is the codec's *last* resort, so writing loopback there
    /// would mean registering a declaration that is not the one being ported,
    /// and `auth.base_url` is only ever written by an auth hook.
    fn open(
        slug: &'a str,
        fixture: &'a Fixture,
        build: impl FnOnce() -> (Box<dyn Provider>, G),
    ) -> Self {
        let home = TempDir::new().expect(TEMPDIR_FAILED);
        for var in HOME_VARS {
            unsafe { std::env::set_var(var, home.path()) };
        }
        let key_env = ProviderRegistry::get(slug)
            .expect(NOT_A_BUILTIN)
            .api_key_env;
        unsafe { std::env::set_var(key_env, API_KEY) };

        let (base_url, requests) = serve(fixture.script);
        unsafe { std::env::set_var(base_url_env_var(slug), base_url) };
        let (provider, guard) = build();
        Self {
            provider,
            requests,
            slug,
            fixture,
            _world: (guard, home),
        }
    }

    fn stream(self, model: &Model, messages: &[Message], tools: &Value) {
        let (events, result) = self.exchange(model, messages, tools);
        self.assert(&json!({
            REQUESTS_KEY: self.recorded(),
            EVENTS_KEY: events,
            OUTCOME_KEY: outcome(&result),
        }));
    }

    fn usage(self) {
        let before = Timestamp::now();
        let result = smol::block_on(self.provider.fetch_usage());
        let after = Timestamp::now();
        let mut observed = json!({
            REQUESTS_KEY: self.recorded(),
            OUTCOME_KEY: match &result {
                Ok(usage) => json!({ USAGE_KEY: usage }),
                Err(e) => failure(e),
            },
        });
        normalise_clock(&mut observed, &[before, after]);
        self.assert(&observed);
    }

    fn models(self) {
        let listed = smol::block_on(self.provider.list_models());
        self.assert(&json!({
            REQUESTS_KEY: self.recorded(),
            OUTCOME_KEY: listing(&listed),
        }));
    }

    fn discovered(self, model: &Model) {
        let listed = smol::block_on(self.provider.list_models());
        if let Ok(models) = &listed {
            model_registry::set_known_models(self.slug, models.clone());
        }
        let (events, result) = self.exchange(model, &turn(), &tools());
        self.assert(&json!({
            REQUESTS_KEY: self.recorded(),
            DISCOVERY_KEY: listing(&listed),
            EVENTS_KEY: events,
            OUTCOME_KEY: outcome(&result),
        }));
    }

    fn exchange(
        &self,
        model: &Model,
        messages: &[Message],
        tools: &Value,
    ) -> (Vec<ProviderEvent>, Result<StreamResponse, AgentError>) {
        let session = self
            .fixture
            .session
            .map(|raw| raw.parse::<SessionRef>().expect(BAD_SESSION));
        let (tx, rx) = flume::unbounded();
        let result = smol::block_on(self.provider.stream_message(
            model,
            messages,
            SYSTEM,
            tools,
            &tx,
            RequestOptions {
                thinking: self.fixture.thinking,
                fast: false,
            },
            session.as_ref(),
        ));
        drop(tx);
        (rx.drain().collect(), result)
    }

    /// Arrival order, unless the script is routed. Routed requests raced each
    /// other, so they are listed by path, and the stable sort keeps arrival
    /// order only among requests for the same path.
    fn recorded(&self) -> Value {
        let requests = self.requests.lock().unwrap();
        let mut listed: Vec<&Recorded> = requests.iter().collect();
        if is_routed(self.fixture.script) {
            listed.sort_by(|a, b| a.path.cmp(&b.path));
        }
        Value::Array(listed.into_iter().map(request_value).collect())
    }

    fn assert(&self, observed: &Value) {
        assert_golden(self.slug, self.fixture, observed);
    }
}

fn listing(listed: &Result<Vec<ModelInfo>, AgentError>) -> Value {
    match listed {
        Ok(models) => json!({ MODELS_KEY: models }),
        Err(e) => failure(e),
    }
}

/// Takes the wall clock out of a usage observation without a clock seam in
/// production: today's UTC date inside a request path becomes `<today>`, and
/// a `reset_at` equal to the next UTC midnight becomes `<next-utc-midnight>`.
///
/// `samples` are read before and after the call and each is accepted, so a
/// run straddling midnight cannot flake. Only exact matches are replaced, so
/// a provider that dates by the local timezone, or is a day off, still shows
/// the date it sent and fails.
fn normalise_clock(observed: &mut Value, samples: &[Timestamp]) {
    let days: Vec<(String, u64)> = samples.iter().map(|&at| utc_day(at)).collect();
    if let Some(requests) = observed.get_mut(REQUESTS_KEY).and_then(Value::as_array_mut) {
        for path in requests
            .iter_mut()
            .filter_map(|request| request.get_mut(PATH_KEY))
        {
            if let Some(raw) = path.as_str() {
                let replaced = days
                    .iter()
                    .fold(raw.to_owned(), |dated, (day, _)| dated.replace(day, TODAY));
                *path = Value::String(replaced);
            }
        }
    }
    if let Some(limits) = observed
        .pointer_mut(LIMITS_POINTER)
        .and_then(Value::as_array_mut)
    {
        for reset_at in limits
            .iter_mut()
            .filter_map(|limit| limit.get_mut(RESET_AT_KEY))
        {
            if reset_at
                .as_u64()
                .is_some_and(|at| days.iter().any(|&(_, midnight)| midnight == at))
            {
                *reset_at = Value::String(NEXT_UTC_MIDNIGHT.to_owned());
            }
        }
    }
}

/// The UTC date `at` falls on, as `YYYY-MM-DD`, and the epoch milliseconds of
/// the midnight that ends it.
fn utc_day(at: Timestamp) -> (String, u64) {
    let day = at.to_zoned(TimeZone::UTC).date();
    let midnight = day
        .tomorrow()
        .and_then(|next| next.to_zoned(TimeZone::UTC))
        .expect(NO_MIDNIGHT);
    (
        day.to_string(),
        midnight.timestamp().as_millisecond() as u64,
    )
}

/// The request as the observation keeps it: method, path, the header set and
/// the body verbatim.
///
/// The body stays the string the codec wrote rather than a parsed `Value`, so
/// nothing between here and the golden can quietly repair malformed JSON. What
/// reaches disk is canonicalised instead, see [`canonical_observation`].
fn request_value(recorded: &Recorded) -> Value {
    let headers: BTreeMap<&str, &str> = recorded
        .headers
        .iter()
        .map(|(name, value)| {
            let value = if is_volatile(name) {
                VOLATILE_VALUE
            } else {
                value.as_str()
            };
            (name.as_str(), value)
        })
        .collect();
    json!({
        "method": recorded.method,
        "path": recorded.path,
        "headers": headers,
        "body": String::from_utf8_lossy(&recorded.body),
    })
}

/// `host` carries the loopback port the kernel happened to hand out and
/// `user-agent` carries the build's git hash, so for those two the comparison
/// is that the header was sent at all.
fn is_volatile(name: &str) -> bool {
    name == HOST_HEADER || name == USER_AGENT_HEADER
}

fn outcome(result: &Result<StreamResponse, AgentError>) -> Value {
    match result {
        Ok(response) => json!({
            "message": response.message,
            "usage": response.usage,
            "stop_reason": response.stop_reason,
            // The one number a session's gauge takes from a response, and the
            // separate usage fields above do not show which of them it sums.
            "context_size": response.usage.total_input(),
        }),
        Err(e) => failure(e),
    }
}

/// `AgentError` cannot be `PartialEq`, so [`AgentError::projection`] is the
/// comparison. `error.rs` carries a test proving two equal projections agree
/// on every observable predicate, which a hand-rolled `(discriminant, status,
/// message)` tuple would not. It is written through `Debug` because the
/// projection is a structural enum over `PartialEq` fields, so its debug form
/// separates exactly what `==` does.
///
/// The rendered message rides along because the projection reads the message
/// only through those predicates, and some behaviour lives nowhere else: a
/// provider that substitutes a message for an error frame that carried none
/// projects identically to one that does not.
fn failure(e: &AgentError) -> Value {
    json!({ "error": format!("{:?}", e.projection()), "message": e.to_string() })
}

fn golden_path(provider: &str, case: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(GOLDEN_DIR)
        .join(provider)
        .join(format!("{case}.json"))
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).expect(NOT_AN_OBJECT)
}

/// Sends the observation through text once, the same trip the golden file
/// takes. serde_json is built without `float_roundtrip`, so
/// `0.024999999999999998` reads back as `0.025`, and a float could otherwise
/// differ from its own recording.
fn through_text(observed: &Value) -> Value {
    serde_json::from_str(&observed.to_string()).expect(NOT_AN_OBJECT)
}

/// The observation as an artifact on disk: the same file whatever else was in
/// the build.
///
/// `serde_json::Map` is an `IndexMap` whenever anything in the build graph
/// turns on `preserve_order`. `agent-client-protocol-schema` does, so a
/// workspace build has it and `-p maki-providers` does not, and cargo unifies
/// features across the graph rather than per crate. Key order would then be a
/// property of the `-p` flags, both in the golden itself and inside the
/// recorded body, which is a JSON document carried as a string. Sorting every
/// object at every depth, and the body's after parsing it, leaves one
/// canonical form for both builds to agree on.
fn canonical_observation(observed: &Value) -> Value {
    let mut canonical = sorted(observed);
    let requests = canonical
        .get_mut(REQUESTS_KEY)
        .and_then(Value::as_array_mut)
        .expect(NO_REQUESTS);
    for request in requests {
        let Some(body) = request.get_mut(BODY_KEY) else {
            continue;
        };
        // A body that is not JSON, or is empty, keeps the raw string: there is
        // no key order in it to leak.
        if let Some(parsed) = body
            .as_str()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        {
            *body = Value::String(serde_json::to_string(&sorted(&parsed)).expect(NOT_AN_OBJECT));
        }
    }
    canonical
}

fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), sorted(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        scalar => scalar.clone(),
    }
}

/// Compares one observation against the artifact on disk, or records it when
/// `UPDATE_GOLDENS=1`. Both sides are canonicalised, so this asserts what the
/// provider did and never which crates the test binary was linked against.
fn assert_golden(provider: &str, fixture: &Fixture, observed: &Value) {
    let path = golden_path(provider, fixture.name);
    let observed = canonical_observation(&through_text(observed));
    if std::env::var(UPDATE_ENV).is_ok_and(|value| value == UPDATE_ON) {
        std::fs::create_dir_all(path.parent().expect(NO_GOLDEN_DIR)).expect(WRITE_FAILED);
        std::fs::write(&path, pretty(&observed) + "\n").expect(WRITE_FAILED);
        return;
    }
    let recorded = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "no golden at {}: {e}\nrecord it with {UPDATE_ENV}={UPDATE_ON}",
            path.display()
        )
    });
    let expected =
        canonical_observation(&serde_json::from_str::<Value>(&recorded).expect(BAD_GOLDEN));
    assert!(
        observed == expected,
        "{} drifted from {}\n--- recorded\n{}\n--- observed\n{}",
        fixture.name,
        path.display(),
        pretty(&expected),
        pretty(&observed)
    );
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const LIMITS_KEY: &str = "limits";
    const NOON: &str = "2026-09-23T12:00:00Z";
    const BEFORE_MIDNIGHT: &str = "2026-09-23T23:59:59Z";
    const AFTER_MIDNIGHT: &str = "2026-09-24T00:00:01Z";
    const SEPTEMBER_24_MS: u64 = 1_790_208_000_000;
    const SEPTEMBER_25_MS: u64 = 1_790_294_400_000;
    const HOUR_MS: u64 = 3_600_000;
    const DATED_PATH: &str = "/global/activity?start_date=2026-09-23&end_date=2026-09-23";
    const NEXT_DATED_PATH: &str = "/spend/logs/v2?start_date=2026-09-24&end_date=2026-09-24";
    const STALE_PATH: &str = "/global/activity?start_date=2026-09-22&end_date=2026-09-22";

    fn observation(path: &str, reset_at: u64) -> Value {
        json!({
            REQUESTS_KEY: [{ PATH_KEY: path }],
            OUTCOME_KEY: { USAGE_KEY: { LIMITS_KEY: [{ RESET_AT_KEY: reset_at }] } },
        })
    }

    #[test_case(NOON, NOON, DATED_PATH, SEPTEMBER_24_MS,
        "/global/activity?start_date=<today>&end_date=<today>", json!(NEXT_UTC_MIDNIGHT) ; "today_and_its_midnight")]
    #[test_case(BEFORE_MIDNIGHT, AFTER_MIDNIGHT, NEXT_DATED_PATH, SEPTEMBER_25_MS,
        "/spend/logs/v2?start_date=<today>&end_date=<today>", json!(NEXT_UTC_MIDNIGHT) ; "straddle_accepts_the_later_day")]
    #[test_case(BEFORE_MIDNIGHT, AFTER_MIDNIGHT, DATED_PATH, SEPTEMBER_24_MS,
        "/global/activity?start_date=<today>&end_date=<today>", json!(NEXT_UTC_MIDNIGHT) ; "straddle_accepts_the_earlier_day")]
    #[test_case(NOON, NOON, STALE_PATH, SEPTEMBER_24_MS - 2 * HOUR_MS,
        STALE_PATH, json!(SEPTEMBER_24_MS - 2 * HOUR_MS) ; "wrong_day_and_local_midnight_stay")]
    fn clock_is_normalised_on_exact_matches_only(
        before: &str,
        after: &str,
        path: &str,
        reset_at: u64,
        expected_path: &str,
        expected_reset_at: Value,
    ) {
        let mut observed = observation(path, reset_at);
        normalise_clock(
            &mut observed,
            &[before.parse().unwrap(), after.parse().unwrap()],
        );
        assert_eq!(observed[REQUESTS_KEY][0][PATH_KEY], expected_path);
        assert_eq!(
            observed.pointer(LIMITS_POINTER).unwrap()[0][RESET_AT_KEY],
            expected_reset_at
        );
    }
}
