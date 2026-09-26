//! A Lua-registered provider, end to end.
//!
//! The fixture in `tests/fixtures/provider_plugin` is loaded into a real plugin
//! host and driven through the [`Provider`] trait against recorded transcripts
//! served on loopback, so every hook is exercised the way a request exercises
//! it. A few plugins written inline cover what the fixture cannot: the
//! responses codec, a credential write racing another maki process, and a hook
//! still running when its plugin is reloaded.
//!
//! The provider registry and the process environment are both global, which is
//! why each test here boots its own host and leans on `cargo nextest` giving
//! every test its own process.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use maki_agent::tools::ToolRegistry;
use maki_lua::{PluginHost, PluginPermissions};
use maki_providers::model::{Model, ModelTier};
use maki_providers::provider::Provider;
use maki_providers::retry::RetryKind;
use maki_providers::test_support::{Canned, JSON_HEADERS, Recorded, Requests, serve};
use maki_providers::{
    AgentError, Effort, Message, ProviderEvent, RequestOptions, StopReason, StreamResponse,
    ThinkingConfig, Timeouts, plugin,
};
use maki_storage::StateDir;
use maki_storage::auth::{load_plugin_auth, lock_plugin_store};
use serde_json::{Value, json};
use tempfile::TempDir;
use test_case::test_case;

const SLUG: &str = "acmelua";
const DISPLAY_NAME: &str = "Acme (Lua)";
const MODEL: &str = "acme-1";
/// The origin a user points the slug at, which is the one a hook's side
/// requests reach unguarded, loopback included.
const BASE_URL_ENV: &str = "ACMELUA_BASE_URL";
const MODELS_PATH: &str = "/models";
const LISTED_WINDOW: u32 = 128000;
const LOOPBACK_HOST: &str = "127.0.0.1";
const TOKEN_KEY: &str = "token";
const ANON_TOKEN: &str = "anonymous";
const RENEWED_TOKEN: &str = "anonymous-renewed";
/// What the recorded server answers a rate limit with, and the same seconds
/// the error is expected to carry back out. One literal, so the header and the
/// expectation cannot drift apart.
const RETRY_AFTER: &str = "7";

/// Every plugin written inline loads under this one name, so loading a second
/// source is a reload of the first.
const INLINE_PLUGIN: &str = "acme_inline";

const RESPONSES_SLUG: &str = "acmeresponses";
const RESPONSES_MODEL: &str = "acme-r1";
const RESPONSES_MARKER: &str = "responses";

const PARKING_SLUG: &str = "acmeparked";
const PARKING_HOST: &str = "api.acme.example";
const PARKING_POLL_MS: u64 = 10;
const LOCKED_TOKEN: &str = "locked";
const LOCKED_SLUG: &str = "acmelocked";
const FREE_SLUG: &str = "acmefree";
/// Generous on purpose. Nothing waited on here takes a whole second on any
/// machine, so spending this much means it is stuck for good rather than slow.
const STUCK_AFTER: Duration = Duration::from_secs(20);
const RELOADED_SOURCE: &str = "-- the provider plugin, reloaded without its registration\n";

/// A provider maki ships a declaration for, so the slug is both a built-in row
/// and a decl already standing when the plugin below reaches for it.
const BUILTIN_SLUG: &str = "deepseek";
const BUILTIN_HOST: &str = "api.deepseek.com";
const RESERVED_SLUG_MESSAGE: &str = "belongs to a built-in provider";
const CLAIM_ALLOWED: &str = "a third-party plugin took a built-in slug";
const BUILTIN_TAKEN: &str = "a refused declaration must not be serving the slug";

const PROMPT: &str = "read a.txt";
const SYSTEM: &str = "You are a test.";
/// The `system_prefix` the fixture registers, which only reaches the wire if
/// the openai codec honours the field rather than dropping it.
const SYSTEM_PREFIX: &str = "Acme house rules: answer in full sentences.";
const REMAPPED_MESSAGE: &str = "Acme allowance is spent until the next cycle";

const HOST_FAILED: &str = "the plugin host did not start";
const HOOK_NEVER_SIGNALLED: &str = "the login hook never reached the point the test waits on";
const BAD_RETRY_AFTER: &str = "the recorded retry-after is not a number of seconds";
const HOOK_THREAD_FAILED: &str = "the thread running the login hook panicked";
const LOAD_FAILED: &str = "the provider plugin did not load";
const CREATE_FAILED: &str = "the registered provider could not be built";
const UNKNOWN_MODEL: &str = "the registered model table has no such model";
const STREAM_FAILED: &str = "the recorded transcript did not stream";
const HOOK_FAILED: &str = "a provider hook did not answer";
const NO_USAGE: &str = "fetch_usage answered with nothing";
const NO_STATE_DIR: &str = "the isolated state directory did not resolve";
const NO_CREDENTIALS: &str = "the refresh hook stored no credentials";
const TEMPDIR_FAILED: &str = "no temporary state directory";
const IO_FAILED: &str = "the recorded connection broke";
const NO_TOKEN_LOCK: &str = "the credential lock could not be taken";
const HOST_THREAD_PARKED: &str =
    "a credential write parked the plugin host, so no other hook could be served";

/// The level the request asks for, and the one the model's declared levels snap
/// it onto. They differ on purpose: only `apply_thinking` can put the snapped
/// one in the body, so a hook that reports it must have run after it.
const ASKED_EFFORT: Effort = Effort::Max;
const SNAPPED_EFFORT: &str = "high";

const CHAT_TRANSCRIPT: &str = r#"data: {"choices":[{"delta":{"reasoning_content":"weighing the options"}}]}

data: {"choices":[{"delta":{"content":"Hello"}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"path\":"}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}

data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":12,"completion_tokens":5}}

data: [DONE]

"#;

const RESPONSES_TRANSCRIPT: &str = r#"event: response.output_text.delta
data: {"delta":"Hello"}

event: response.completed
data: {"response":{"status":"completed","usage":{"input_tokens":12,"output_tokens":5}}}

"#;

const EXPIRED_TOKEN_BODY: &str = r#"{"error":{"message":"token expired"}}"#;
const ALLOWANCE_BODY: &str = r#"{"error":{"message":"monthly allowance exhausted"}}"#;
const OVERLOADED_BODY: &str = r#"{"error":{"message":"upstream is busy"}}"#;

const SLOW_DOWN_HEADERS: &[(&str, &str)] = &[
    ("content-type", "application/json"),
    ("retry-after", RETRY_AFTER),
];

const CHAT_SCRIPT: &[Canned] = &[Canned::sse(CHAT_TRANSCRIPT)];
const RESPONSES_SCRIPT: &[Canned] = &[Canned::sse(RESPONSES_TRANSCRIPT)];
const NO_REQUESTS: &[Canned] = &[];
const MODELS_BODY: &str = r#"{"data":[{"id":"acme-1","context_length":128000}]}"#;
const MODELS_SCRIPT: &[Canned] = &[Canned::json(200, MODELS_BODY)];
const REFRESH_SCRIPT: &[Canned] = &[
    Canned {
        status: 401,
        headers: JSON_HEADERS,
        body: EXPIRED_TOKEN_BODY,
        path: None,
    },
    Canned::sse(CHAT_TRANSCRIPT),
    Canned::sse(CHAT_TRANSCRIPT),
];
const ALLOWANCE_SCRIPT: &[Canned] = &[Canned {
    status: 429,
    headers: SLOW_DOWN_HEADERS,
    body: ALLOWANCE_BODY,
    path: None,
}];
const OVERLOADED_SCRIPT: &[Canned] = &[Canned {
    status: 503,
    headers: SLOW_DOWN_HEADERS,
    body: OVERLOADED_BODY,
    path: None,
}];

/// Points every base directory at a throwaway tree, so the credentials the
/// fixture's `login` and `auth` store never touch the real state dir.
fn isolated_state() -> TempDir {
    let dir = TempDir::new().expect(TEMPDIR_FAILED);
    for var in [
        "HOME",
        "XDG_STATE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
    ] {
        unsafe { std::env::set_var(var, dir.path()) };
    }
    dir
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/provider_plugin/init.lua")
}

/// A grant to reach exactly one host, the way a `plugin.toml` with a
/// `net_hosts` line grants it.
fn permissions_for(host: &str) -> PluginPermissions {
    let mut permissions = PluginPermissions::trusted();
    permissions.set_net_hosts(Some(Arc::from(vec![host.to_owned()])));
    permissions
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

fn plugin_host() -> PluginHost {
    PluginHost::new(Arc::new(ToolRegistry::new())).expect(HOST_FAILED)
}

fn load_inline(host: &PluginHost, source: &str, net_host: &str) {
    host.load_source_with_permissions(INLINE_PLUGIN, source, permissions_for(net_host))
        .expect(LOAD_FAILED);
}

/// The fixture plugin, loaded from disk with the grant its `plugin.toml`
/// declares, talking to a recorded server instead of Acme.
struct Fixture {
    provider: Box<dyn Provider>,
    server: Requests,
    _state: TempDir,
    _host: PluginHost,
}

impl Fixture {
    fn start(script: &'static [Canned]) -> Self {
        let (base_url, server) = serve(script);
        Self::at(&base_url, server)
    }

    fn at(base_url: &str, server: Requests) -> Self {
        let state = isolated_state();
        unsafe { std::env::set_var(BASE_URL_ENV, base_url) };

        let host = plugin_host();
        host.load_plugin_file(&fixture_path()).expect(LOAD_FAILED);
        plugin::commit_load();

        Self {
            provider: plugin::create(SLUG, Timeouts::default()).expect(CREATE_FAILED),
            server,
            _state: state,
            _host: host,
        }
    }

    fn stream(
        &self,
        thinking: ThinkingConfig,
    ) -> (Vec<ProviderEvent>, Result<StreamResponse, AgentError>) {
        let model = plugin::lookup_model(SLUG, MODEL).expect(UNKNOWN_MODEL);
        stream(self.provider.as_ref(), &model, thinking)
    }

    fn requests(&self) -> Vec<Value> {
        self.server
            .lock()
            .unwrap()
            .iter()
            .map(Recorded::json)
            .collect()
    }

    fn tokens(&self) -> Vec<String> {
        self.server
            .lock()
            .unwrap()
            .iter()
            .map(|recorded| recorded.authorization().to_owned())
            .collect()
    }
}

/// One turn, and everything it emitted. The only place a test builds a
/// request, so a provider built by hand asks the same question the fixture
/// does.
fn stream(
    provider: &dyn Provider,
    model: &Model,
    thinking: ThinkingConfig,
) -> (Vec<ProviderEvent>, Result<StreamResponse, AgentError>) {
    let messages = [Message::user(PROMPT.to_owned())];
    let (tx, rx) = flume::unbounded();
    let result = smol::block_on(provider.stream_message(
        model,
        &messages,
        SYSTEM,
        &json!([]),
        &tx,
        RequestOptions {
            thinking,
            fast: false,
        },
        None,
    ));
    drop(tx);
    (rx.drain().collect(), result)
}

/// One turn, both ways round: the events a transcript produces in the order the
/// ui would render them, and the body that asked for them.
///
/// `build_body` is handed the request after `apply_thinking` ran, so the hook's
/// own key carries the snapped effort that only the finished body holds, and
/// the key the hook deleted never reaches the wire.
#[test]
fn a_recorded_turn_streams_its_events_and_posts_the_body_the_hook_built() {
    let fixture = Fixture::start(CHAT_SCRIPT);

    let (events, result) = fixture.stream(ThinkingConfig::Effort(ASKED_EFFORT));
    let response = result.expect(STREAM_FAILED);

    assert_eq!(
        events,
        [
            ProviderEvent::ThinkingDelta {
                text: "weighing the options".to_owned()
            },
            ProviderEvent::TextDelta {
                text: "Hello".to_owned()
            },
            ProviderEvent::ToolUseStart {
                id: "call_1".to_owned(),
                name: "read".to_owned()
            },
        ]
    );
    assert_eq!(response.stop_reason, Some(StopReason::ToolUse));
    assert_eq!((response.usage.input, response.usage.output), (12, 5));

    let sent = fixture.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(fixture.tokens(), [bearer(ANON_TOKEN)]);
    assert_eq!(sent[0]["model"], json!(MODEL));
    assert_eq!(
        sent[0]["messages"][0]["content"],
        json!(format!("{SYSTEM_PREFIX}\n\n{SYSTEM}"))
    );
    assert_eq!(
        sent[0]["acme_reasoning"],
        json!({ "model": MODEL, "effort": SNAPPED_EFFORT, "asked_for": ASKED_EFFORT.as_str() })
    );
    assert!(
        sent[0].get("reasoning_effort").is_none(),
        "the hook removed reasoning_effort, so it must not be on the wire: {}",
        sent[0]
    );
}

/// All three auth purposes, each observed through the token that reached the
/// server: `resolve` before the first request, `refresh` after a 401 that
/// preceded every event, and `reload` re-reading the store, which by then
/// holds the token the refresh minted and wrote.
#[test]
fn the_auth_hooks_drive_the_credential_lifecycle() {
    let fixture = Fixture::start(REFRESH_SCRIPT);

    fixture.stream(ThinkingConfig::Off).1.expect(STREAM_FAILED);
    smol::block_on(fixture.provider.reload_auth()).expect(HOOK_FAILED);
    fixture.stream(ThinkingConfig::Off).1.expect(STREAM_FAILED);

    assert_eq!(
        fixture.tokens(),
        [
            bearer(ANON_TOKEN),
            bearer(RENEWED_TOKEN),
            bearer(RENEWED_TOKEN),
        ]
    );
}

/// The host holds this provider's credential lock while a refresh runs, so
/// a hook that stores the token it minted has to be let back in through that
/// same lock. A hook that waits on its own caller instead never answers, and
/// the call below comes back as a hook timeout rather than as a token.
#[test]
fn a_refresh_hook_persists_the_token_it_minted() {
    let fixture = Fixture::start(NO_REQUESTS);

    smol::block_on(fixture.provider.refresh_auth()).expect(HOOK_FAILED);

    let dir = StateDir::resolve().expect(NO_STATE_DIR);
    let stored = load_plugin_auth(&dir, SLUG).expect(NO_CREDENTIALS);
    assert_eq!(stored[TOKEN_KEY], json!(RENEWED_TOKEN));
}

/// Both answers come from hooks rather than from the static registration: the
/// listing is what the server answered `ctx.get_json`, asked with the token
/// every request carries, and the output window the `models` table states is
/// absent from it.
#[test]
fn list_models_and_fetch_usage_answer_from_their_hooks() {
    let fixture = Fixture::start(MODELS_SCRIPT);

    let models = smol::block_on(fixture.provider.list_models()).expect(HOOK_FAILED);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, MODEL);
    assert_eq!(models[0].context_window, Some(LISTED_WINDOW));
    assert_eq!(models[0].tier, Some(ModelTier::Strong));
    assert_eq!(models[0].max_output_tokens, None);
    let asked = fixture.server.lock().unwrap();
    assert_eq!(asked.len(), 1);
    assert!(asked[0].path.ends_with(MODELS_PATH), "{}", asked[0].path);
    assert_eq!(asked[0].authorization(), bearer(ANON_TOKEN));
    drop(asked);

    let usage = smol::block_on(fixture.provider.fetch_usage())
        .expect(HOOK_FAILED)
        .expect(NO_USAGE);
    assert_eq!(usage.plan.as_deref(), Some("team"));
    assert_eq!(usage.limits.len(), 1);
    assert_eq!(usage.limits[0].percentage, Some(42));
}

/// The hook restates status and message and nothing else. Retryability is
/// re-derived from the status it handed back, while `Retry-After` is what the
/// server asked for and survives either way.
#[test_case(ALLOWANCE_SCRIPT, 400, REMAPPED_MESSAGE, None ; "a_mapped_status_stops_the_retry")]
#[test_case(OVERLOADED_SCRIPT, 503, OVERLOADED_BODY, Some(RetryKind::Transient) ; "an_unmapped_status_passes_through")]
fn map_error_restates_the_status_and_keeps_retry_after(
    script: &'static [Canned],
    expected_status: u16,
    expected_message: &str,
    expected_kind: Option<RetryKind>,
) {
    let fixture = Fixture::start(script);

    let error = fixture.stream(ThinkingConfig::Off).1.unwrap_err();

    let AgentError::Api {
        status, message, ..
    } = &error
    else {
        panic!("expected an api error, got {error:?}");
    };
    assert_eq!(*status, expected_status);
    assert_eq!(message, expected_message);
    assert_eq!(error.retry_kind(), expected_kind);
    let asked_for = Duration::from_secs(RETRY_AFTER.parse().expect(BAD_RETRY_AFTER));
    assert_eq!(error.retry_after(), Some(asked_for));
}

/// `ctx.get_json` hands back a refused request as the error the codec would
/// raise for it, `Retry-After` included.
#[test]
fn a_refused_side_request_is_the_native_error() {
    let fixture = Fixture::start(OVERLOADED_SCRIPT);

    let error = smol::block_on(fixture.provider.list_models()).unwrap_err();

    assert!(
        matches!(error, AgentError::Api { status: 503, .. }),
        "{error:?}"
    );
    let asked_for = Duration::from_secs(RETRY_AFTER.parse().expect(BAD_RETRY_AFTER));
    assert_eq!(error.retry_after(), Some(asked_for));
}

/// A side request that never reached a server reads as the transport failure
/// it is, not as a broken hook.
#[test]
fn a_side_request_that_cannot_connect_is_a_transport_error() {
    let closed = TcpListener::bind((LOOPBACK_HOST, 0)).expect(IO_FAILED);
    let base_url = format!("http://{}", closed.local_addr().expect(IO_FAILED));
    drop(closed);
    let fixture = Fixture::at(&base_url, Requests::default());

    let error = smol::block_on(fixture.provider.list_models()).unwrap_err();

    assert!(matches!(error, AgentError::Http(_)), "{error:?}");
}

/// There is no `has_auth` flag: defining `login` is the whole of what makes a
/// plugin provider an auth target, and both halves run against a real host.
#[test]
fn a_login_hook_is_what_makes_the_slug_an_auth_target() {
    let _fixture = Fixture::start(NO_REQUESTS);

    assert!(
        plugin::auth_providers().contains(&(SLUG.to_owned(), DISPLAY_NAME.to_owned())),
        "{:?}",
        plugin::auth_providers()
    );
    plugin::login(SLUG).expect(HOOK_FAILED);
    plugin::logout(SLUG).expect(HOOK_FAILED);
}

fn responses_plugin(base_url: &str) -> String {
    format!(
        r#"
maki.provider.register({{
  slug = "{RESPONSES_SLUG}",
  display_name = "Acme Responses",
  codec = "openai-responses",
  base_url = "{base_url}",
  models = {{ {{ prefixes = {{ "{RESPONSES_MODEL}" }} }} }},
  build_body = function(_, body)
    body.acme_marker = "{RESPONSES_MARKER}"
    return body
  end,
}})
"#
    )
}

/// `build_body` is threaded through both openai codecs, and the responses
/// branch builds its body somewhere else entirely, so it gets its own case.
#[test]
fn the_responses_codec_applies_the_body_hook_too() {
    let _state = isolated_state();
    let (base_url, server) = serve(RESPONSES_SCRIPT);
    let host = plugin_host();
    load_inline(&host, &responses_plugin(&base_url), LOOPBACK_HOST);
    plugin::commit_load();

    let provider = plugin::create(RESPONSES_SLUG, Timeouts::default()).expect(CREATE_FAILED);
    let model = plugin::lookup_model(RESPONSES_SLUG, RESPONSES_MODEL).expect(UNKNOWN_MODEL);

    let (events, result) = stream(provider.as_ref(), &model, ThinkingConfig::Off);
    let response = result.expect(STREAM_FAILED);

    assert_eq!(
        events,
        [ProviderEvent::TextDelta {
            text: "Hello".to_owned()
        }]
    );
    assert_eq!(response.stop_reason, Some(StopReason::EndTurn));

    let sent = server.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].json()["acme_marker"], json!(RESPONSES_MARKER));
}

/// One provider whose `login` runs `body`. The registration is the same every
/// time, since what these tests vary is only what the hook does once it is
/// called.
fn login_plugin(slug: &str, body: &str) -> String {
    format!(
        r#"
maki.provider.register({{
  slug = "{slug}",
  display_name = "{slug}",
  codec = "openai",
  base_url = "https://{PARKING_HOST}/v1",
  models = {{ {{ prefixes = {{ "{MODEL}" }} }} }},
  login = function()
{body}
  end,
}})
"#
    )
}

/// Parks until the test drops a file, so a reload can land while the call is
/// genuinely in flight.
fn park_until_released(started: &Path, release: &Path) -> String {
    let started = started.display();
    let release = release.display();
    format!(
        r#"    maki.fs.write("{started}", "1")
    while not maki.fs.read("{release}") do
      maki.async.sleep({PARKING_POLL_MS})
    end"#
    )
}

/// Writes the credential store from inside a hook, which is the call the test
/// below wants to catch parking the host.
fn write_credentials(entered: &Path, done: &Path) -> String {
    let entered = entered.display();
    let done = done.display();
    format!(
        r#"    maki.fs.write("{entered}", "1")
    maki.provider.auth.set("{LOCKED_SLUG}", {{ token = "{LOCKED_TOKEN}" }})
    maki.fs.write("{done}", "1")"#
    )
}

/// Waits on a file a Lua hook writes, which is the only line a hook running on
/// the host's own thread can hand back to the test. Polling rather than timing:
/// nothing here is expected to take a set amount of time, and the deadline only
/// turns a hang into a failure the runner can report.
fn wait_for(path: &Path) {
    let deadline = Instant::now() + STUCK_AFTER;
    while !path.exists() {
        assert!(Instant::now() < deadline, "{HOOK_NEVER_SIGNALLED}");
        std::thread::sleep(Duration::from_millis(PARKING_POLL_MS));
    }
}

/// The defect that keeps the credential store off the plugin host's thread:
/// `lock_credentials` waits on a file lock another maki process may hold, and
/// the host is single threaded, so waiting for it inline stalls every other
/// hook, tool call and timer in the process.
///
/// The lock here is held on a second file descriptor, which is what a second
/// maki process looks like to `flock`, and the in-process re-entrancy that lets
/// a refreshing `auth` hook persist its own token deliberately does not cover
/// it.
///
/// `login` carries no hook timeout, so the free login gets a thread of its own.
/// A parked host would otherwise hang the test for good instead of failing it.
#[test]
fn a_credential_write_does_not_park_the_plugin_host() {
    let state = isolated_state();
    let entered = state.path().join("entered");
    let done = state.path().join("done");

    let host = plugin_host();
    let source = format!(
        "{}{}",
        login_plugin(LOCKED_SLUG, &write_credentials(&entered, &done)),
        login_plugin(FREE_SLUG, "")
    );
    load_inline(&host, &source, PARKING_HOST);
    plugin::commit_load();

    let dir = StateDir::resolve().expect(NO_STATE_DIR);
    let held = lock_plugin_store(&dir, LOCKED_SLUG).expect(NO_TOKEN_LOCK);
    let locked = std::thread::spawn(|| plugin::login(LOCKED_SLUG));
    wait_for(&entered);

    let (served, free_login) = flume::bounded(1);
    std::thread::spawn(move || served.send(plugin::login(FREE_SLUG)));
    free_login
        .recv_timeout(STUCK_AFTER)
        .expect(HOST_THREAD_PARKED)
        .expect(HOOK_FAILED);

    assert!(!done.exists(), "{HOST_THREAD_PARKED}");
    drop(held);
    locked.join().expect(HOOK_THREAD_FAILED).expect(HOOK_FAILED);
}

/// The hook handles outlive the plugin that registered them: an unload drops
/// the plugin's environment, while the registry entries the running call sits
/// on belong to the handle and go only once nobody holds it.
#[test]
fn an_in_flight_hook_call_survives_a_plugin_reload() {
    let state = isolated_state();
    let started = state.path().join("started");
    let release = state.path().join("release");

    let host = plugin_host();
    let parking = login_plugin(PARKING_SLUG, &park_until_released(&started, &release));
    load_inline(&host, &parking, PARKING_HOST);
    plugin::commit_load();

    let login = std::thread::spawn(|| plugin::login(PARKING_SLUG));
    wait_for(&started);
    load_inline(&host, RELOADED_SOURCE, PARKING_HOST);
    std::fs::write(&release, "1").expect(IO_FAILED);

    login.join().expect(HOOK_THREAD_FAILED).expect(HOOK_FAILED);
}

/// A slug maki ships is maki's to declare, and a plugin from outside the
/// binary may not take it. A decl that claims one inherits its `api_key_env`,
/// so the key the user set for the built-in would be resolved into the
/// claimant's credentials and handed straight to it as every hook's
/// `ctx.headers`, under a name the picker still labels with the built-in's
/// display name. No `net` grant and no host list ever bought
/// that reach.
#[test]
fn a_third_party_plugin_cannot_take_a_builtin_slug() {
    let _state = isolated_state();
    let host = plugin_host();
    plugin::begin_load();

    let error = host
        .load_source_with_permissions(
            INLINE_PLUGIN,
            &format!(r#"maki.provider.register({{ slug = "{BUILTIN_SLUG}", codec = "openai" }})"#),
            permissions_for(BUILTIN_HOST),
        )
        .expect_err(CLAIM_ALLOWED);
    plugin::commit_load();

    assert!(error.to_string().contains(RESERVED_SLUG_MESSAGE), "{error}");
    assert!(!plugin::is_registered(BUILTIN_SLUG), "{BUILTIN_TAKEN}");
}
