//! Golden replay: one recorded exchange per file, replayed through the bundled
//! plugin that claims the slug its directory is named after.
//!
//! A file under `tests/goldens/<slug>/` carries the whole case: the `call`
//! (which endpoint, and for a turn the model, thinking mode, session, history
//! and tools), the `script` the recorded server answers with, and the
//! `expected` observation. Four things about a provider are worth comparing
//! and the observation holds all four: the request bytes it put on the wire,
//! the [`ProviderEvent`]s it emitted in order, the error it failed with, and
//! the usage it came back with.
//!
//! Every expectation was recorded once, against the bespoke impl the plugin
//! replaced, and is never regenerated after that impl is gone: the files are
//! the spec the plugin answers to. Record a *new* case by writing its `call`
//! and `script` and running with `UPDATE_GOLDENS=1`, which fills in only an
//! absent `expected`. Without it an absent `expected` fails, because a suite
//! that records whatever it sees on its first run has asserted nothing.
//!
//! The registry, the environment, the credential store and the discovered
//! models are process-global and some never reset (`AUTH` is append-only per
//! slug), so [`every_golden_replays`] runs each file in a child process of
//! this binary. Nothing one case leaves behind can reach the next.

use std::collections::BTreeMap;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{env, fs, thread};

use jiff::Timestamp;
use jiff::tz::TimeZone;
use maki_config::providers::base_url_env_var;
use maki_lua::PluginHost;
use maki_providers::model::ModelInfo;
use maki_providers::model_registry;
use maki_providers::plugin;
use maki_providers::provider::Provider;
use maki_providers::spec::ProviderRegistry;
use maki_providers::test_support::{Canned, Recorded, Requests, is_routed, serve};
use maki_providers::{
    AgentError, Message, Model, ProviderEvent, RequestOptions, StreamResponse, ThinkingSupport,
    Timeouts,
};
use maki_storage::id::SessionRef;
use maki_storage::sessions::StoredThinking;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::TempDir;

use super::{isolated_state, load_bundled};

const GOLDEN_DIR: &str = "tests/goldens";
const GOLDEN_EXTENSION: &str = "json";
const CASE_ENV: &str = "MAKI_REPLAY_GOLDEN";
const CHILD_TEST: &str = "replay_one_golden";
const CHILD_PASSED: &str = "1 passed";
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

const NO_GOLDENS: &str = "no golden files to replay";
const UNREADABLE_DIR: &str = "the golden directory is unreadable";
const NO_CASE: &str = "the child replay was spawned without a golden";
const NO_SLUG: &str = "a golden sits in a directory named after its slug";
const NO_TEST_BINARY: &str = "the test binary cannot find itself";
const SPAWN_FAILED: &str = "the child replay did not start";
const NOT_A_MODULE: &str = "the golden module is nested inside the test crate";
const WRITE_FAILED: &str = "the golden could not be recorded";
const BAD_GOLDEN: &str = "the golden is not a well-formed case";
const NOT_AN_OBJECT: &str = "an observation is always a json object";
const NO_REQUESTS: &str = "every observation records the requests it sent";
const NOT_A_BUILTIN: &str = "a replayed slug is a builtin";
const CREATE_FAILED: &str = "the provider could not be built";
const UNKNOWN_MODEL: &str = "the model spec did not resolve";
const NO_MIDNIGHT: &str = "the day after a sampled one is representable";

/// One golden file as it sits on disk. `call` and `script` stay raw so a
/// recording writes them back as the author left them.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoldenFile {
    call: Value,
    script: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expected: Option<Value>,
}

/// Which endpoint the case exercises. The field-less ones are struct
/// variants because serde refuses unknown fields only on those.
#[derive(Deserialize)]
#[serde(tag = "endpoint", rename_all = "snake_case", deny_unknown_fields)]
enum Call {
    Stream(Turn),
    /// `list_models`, its rows handed to the registry the way startup does,
    /// then the turn, all drawn from one script. A failed listing is recorded
    /// and the turn still runs, undiscovered.
    Discovered(Turn),
    /// The one endpoint that reads the wall clock, so its observation is
    /// normalised against it, see [`normalise_clock`].
    Usage {},
    /// Each row is recorded whole, so a [`ModelInfo`] field added later lands
    /// in every golden rather than slipping past a hand-picked projection.
    Models {},
}

/// What a turn asks for beyond the fixed system prompt.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Turn {
    model: String,
    /// Pinned rather than left to discovery or the models.dev cache, so a
    /// gate reads the same in every process.
    thinking_override: Option<ThinkingSupport>,
    thinking: StoredThinking,
    session: Option<SessionRef>,
    /// Defaults to the lone [`PROMPT`]. A provider whose body work reads the
    /// history needs turns to act on.
    messages: Option<Vec<Message>>,
    /// Defaults to [`tools`]. What a provider does only when tools are
    /// present is invisible when they always are.
    tools: Option<Value>,
}

impl Turn {
    fn model(&self) -> Model {
        let mut model = Model::from_spec(&self.model).expect(UNKNOWN_MODEL);
        if let Some(support) = self.thinking_override {
            model.thinking_override = Some(support);
        }
        model
    }
}

/// One recorded response. `path` routes it, see [`Canned::at`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
    path: Option<String>,
    status: u16,
    headers: BTreeMap<String, String>,
    body: String,
}

impl Answer {
    /// The server serves `'static` scripts, and a child process replays one
    /// case and exits, so leaking the script is the whole cost of borrowing it.
    fn leak(self) -> Canned {
        let headers: Vec<(&'static str, &'static str)> = self
            .headers
            .into_iter()
            .map(|(name, value)| (&*name.leak(), &*value.leak()))
            .collect();
        Canned {
            status: self.status,
            headers: headers.leak(),
            body: self.body.leak(),
            path: self.path.map(|path| &*path.leak()),
        }
    }
}

/// Replays every golden, each in a child process of this binary, and names
/// every file that failed.
#[test]
fn every_golden_replays() {
    let goldens = goldens();
    assert!(!goldens.is_empty(), "{NO_GOLDENS}");
    let next = AtomicUsize::new(0);
    let failures = Mutex::new(Vec::new());
    let workers = thread::available_parallelism().map_or(1, NonZero::get);
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while let Some(golden) = goldens.get(next.fetch_add(1, Ordering::Relaxed)) {
                    if let Err(report) = replay_in_child(golden) {
                        failures.lock().unwrap().push(report);
                    }
                }
            });
        }
    });
    let mut failures = failures.into_inner().unwrap();
    failures.sort();
    assert!(
        failures.is_empty(),
        "{} of {} goldens failed:\n{}",
        failures.len(),
        goldens.len(),
        failures.join("\n")
    );
}

/// The child half of [`every_golden_replays`]. Ignored, since run on its own
/// it has no golden to replay.
#[test]
#[ignore = "spawned once per golden by every_golden_replays"]
fn replay_one_golden() {
    replay(Path::new(&env::var_os(CASE_ENV).expect(NO_CASE)));
}

fn goldens() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN_DIR);
    let mut goldens: Vec<PathBuf> = fs::read_dir(root)
        .expect(UNREADABLE_DIR)
        .flat_map(|slug| fs::read_dir(slug.expect(UNREADABLE_DIR).path()).expect(UNREADABLE_DIR))
        .map(|case| case.expect(UNREADABLE_DIR).path())
        .filter(|case| case.extension().is_some_and(|ext| ext == GOLDEN_EXTENSION))
        .collect();
    goldens.sort();
    goldens
}

/// A child passes only if it ran the one case, so a renamed child test fails
/// loudly instead of matching nothing and exiting clean.
fn replay_in_child(golden: &Path) -> Result<(), String> {
    let (_, module) = module_path!().split_once("::").expect(NOT_A_MODULE);
    let output = Command::new(env::current_exe().expect(NO_TEST_BINARY))
        .args([
            &format!("{module}::{CHILD_TEST}"),
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .env(CASE_ENV, golden)
        .output()
        .expect(SPAWN_FAILED);
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() && stdout.contains(CHILD_PASSED) {
        return Ok(());
    }
    Err(format!(
        "--- {}\n{stdout}{}",
        golden.display(),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn replay(path: &Path) {
    let text =
        fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let golden: GoldenFile = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{BAD_GOLDEN}: {}: {e}", path.display()));
    let call = Call::deserialize(&golden.call)
        .unwrap_or_else(|e| panic!("{BAD_GOLDEN}: {}: call: {e}", path.display()));
    let script: Vec<Canned> = Vec::<Answer>::deserialize(&golden.script)
        .unwrap_or_else(|e| panic!("{BAD_GOLDEN}: {}: script: {e}", path.display()))
        .into_iter()
        .map(Answer::leak)
        .collect();
    let slug = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|slug| slug.to_str())
        .expect(NO_SLUG);

    let replay = Replay::open(slug, script.leak());
    let observed = match &call {
        Call::Stream(turn) => replay.stream(turn),
        Call::Discovered(turn) => replay.discovered(slug, turn),
        Call::Usage {} => replay.usage(),
        Call::Models {} => replay.models(),
    };
    assert_golden(path, golden, &observed);
}

/// The question every turn asks unless its golden says otherwise, so two
/// providers are never answering different ones.
fn tools() -> Value {
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

/// One exchange ready to run: the provider under test, what the recorded
/// server has seen, and the world both of them live in.
///
/// The world is a guard: the temporary tree is read while the request is
/// built, and a hook whose plugin host has died answers nothing. Fields drop
/// in order, so the provider goes before the host it talks to, and the host
/// before the home it may write to on its way out.
struct Replay {
    provider: Box<dyn Provider>,
    requests: Requests,
    script: &'static [Canned],
    _host: PluginHost,
    _home: TempDir,
}

impl Replay {
    /// Stands up the whole world one exchange needs: a throwaway home with
    /// the key the slug reads and the recorded server, then the bundled plugin
    /// registered inside the load window the way a plugin load does. `create`
    /// resolves the inherited `api_key_env` into a key pool right away, so the
    /// claim on the built-in slug is exercised instead of assumed.
    ///
    /// Loopback is published through `<SLUG>_BASE_URL` because that is the
    /// only rung of the precedence a test can reach. The declaration's own
    /// `base_url` is inherited from the row, and `auth.base_url` is only ever
    /// written by an auth hook.
    fn open(slug: &str, script: &'static [Canned]) -> Self {
        let home = isolated_state();
        let key_env = ProviderRegistry::get(slug)
            .expect(NOT_A_BUILTIN)
            .api_key_env;
        unsafe { env::set_var(key_env, API_KEY) };

        let (base_url, requests) = serve(script);
        unsafe { env::set_var(base_url_env_var(slug), base_url) };
        plugin::begin_load();
        let host = load_bundled(slug);
        plugin::commit_load();
        let provider = plugin::create(slug, Timeouts::default()).expect(CREATE_FAILED);
        Self {
            provider,
            requests,
            script,
            _host: host,
            _home: home,
        }
    }

    fn stream(&self, turn: &Turn) -> Value {
        let (events, result) = self.exchange(&turn.model(), turn);
        json!({
            REQUESTS_KEY: self.recorded(),
            EVENTS_KEY: events,
            OUTCOME_KEY: outcome(&result),
        })
    }

    fn usage(&self) -> Value {
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
        observed
    }

    fn models(&self) -> Value {
        let listed = smol::block_on(self.provider.list_models());
        json!({
            REQUESTS_KEY: self.recorded(),
            OUTCOME_KEY: listing(&listed),
        })
    }

    /// The model is resolved before the listing, as a session holds it
    /// before discovery lands, so what discovery found reaches the turn
    /// through the request path alone.
    fn discovered(&self, slug: &str, turn: &Turn) -> Value {
        let model = turn.model();
        let listed = smol::block_on(self.provider.list_models());
        if let Ok(models) = &listed {
            model_registry::set_known_models(slug, models.clone());
        }
        let (events, result) = self.exchange(&model, turn);
        json!({
            REQUESTS_KEY: self.recorded(),
            DISCOVERY_KEY: listing(&listed),
            EVENTS_KEY: events,
            OUTCOME_KEY: outcome(&result),
        })
    }

    fn exchange(
        &self,
        model: &Model,
        turn: &Turn,
    ) -> (Vec<ProviderEvent>, Result<StreamResponse, AgentError>) {
        let messages = turn
            .messages
            .clone()
            .unwrap_or_else(|| vec![Message::user(PROMPT.to_owned())]);
        let tools = turn.tools.clone().unwrap_or_else(tools);
        let (tx, rx) = flume::unbounded();
        let result = smol::block_on(self.provider.stream_message(
            model,
            &messages,
            SYSTEM,
            &tools,
            &tx,
            RequestOptions {
                thinking: turn.thinking.into(),
                fast: false,
            },
            turn.session.as_ref(),
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
        if is_routed(self.script) {
            listed.sort_by(|a, b| a.path.cmp(&b.path));
        }
        Value::Array(listed.into_iter().map(request_value).collect())
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

fn pretty(value: &impl Serialize) -> String {
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
/// workspace build has it and a narrower `-p` build may not, and cargo unifies
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

/// Compares one observation against the golden's `expected`, or records it
/// when the golden has none and `UPDATE_GOLDENS=1`. Both sides are
/// canonicalised, so this asserts what the provider did and never which
/// crates the test binary was linked against.
fn assert_golden(path: &Path, mut golden: GoldenFile, observed: &Value) {
    let observed = canonical_observation(&through_text(observed));
    let Some(recorded) = &golden.expected else {
        assert!(
            env::var(UPDATE_ENV).is_ok_and(|value| value == UPDATE_ON),
            "{} has no expected observation\nrecord it with {UPDATE_ENV}={UPDATE_ON}",
            path.display()
        );
        golden.expected = Some(observed);
        fs::write(path, pretty(&golden) + "\n").expect(WRITE_FAILED);
        return;
    };
    let expected = canonical_observation(recorded);
    assert!(
        observed == expected,
        "{} drifted\n--- recorded\n{}\n--- observed\n{}",
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
