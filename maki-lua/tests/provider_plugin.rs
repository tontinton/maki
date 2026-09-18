//! A Lua-registered provider, end to end.
//!
//! The fixture in `tests/fixtures/provider_plugin` is loaded into a real plugin
//! host and driven through the [`Provider`] trait against recorded transcripts
//! served on loopback, so every hook is exercised the way a request exercises
//! it. Two plugins written inline cover what the fixture cannot: the responses
//! codec, and a hook still running when its plugin is reloaded.
//!
//! The provider registry and the process environment are both global, which is
//! why each test here boots its own host and leans on `cargo nextest` giving
//! every test its own process.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use maki_agent::tools::ToolRegistry;
use maki_lua::{PluginHost, PluginPermissions};
use maki_providers::model::{Model, ModelTier};
use maki_providers::provider::Provider;
use maki_providers::retry::RetryKind;
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
const BASE_URL_ENV: &str = "ACME_BASE_URL";
const LOOPBACK: &str = "127.0.0.1:0";
const LOOPBACK_HOST: &str = "127.0.0.1";
const REASON_PHRASE: &str = "Recorded";
const TOKEN_KEY: &str = "token";
const RENEWED_TOKEN: &str = "anonymous-renewed";
/// A refresh that parked on the credential lock would spend `HOOK_TIMEOUT`,
/// which is 30 seconds.
const REFRESH_BUDGET: Duration = Duration::from_secs(5);
const RETRY_AFTER_SECONDS: u64 = 7;

const RESPONSES_SLUG: &str = "acmeresponses";
const RESPONSES_MODEL: &str = "acme-r1";
const RESPONSES_MARKER: &str = "responses";

const PARKING_PLUGIN: &str = "acme_parked";
const PARKING_SLUG: &str = "acmeparked";
const PARKING_HOST: &str = "api.acme.example";
const PARKING_POLL_MS: u64 = 10;
const LOCKED_SLUG: &str = "acmelocked";
const FREE_SLUG: &str = "acmefree";
/// Comfortably under `LOCK_WAIT`, which is what a host thread parked on the
/// credential lock would spend.
const UNBLOCKED_BUDGET: Duration = Duration::from_secs(5);
const PARKING_TIMEOUT: Duration = Duration::from_secs(20);
const RELOADED_SOURCE: &str = "-- the provider plugin, reloaded without its registration\n";

const PROMPT: &str = "read a.txt";
const SYSTEM: &str = "You are a test.";
/// The `system_prefix` the fixture registers, which only reaches the wire if
/// the openai codec honours the field rather than dropping it.
const SYSTEM_PREFIX: &str = "Acme house rules: answer in full sentences.";
const REMAPPED_MESSAGE: &str = "Acme allowance is spent until the next cycle";

const HOST_FAILED: &str = "the plugin host did not start";
const NEVER_PARKED: &str = "the login hook never parked";
const HOOK_THREAD_FAILED: &str = "the thread running the login hook panicked";
const LOAD_FAILED: &str = "the provider plugin did not load";
const CREATE_FAILED: &str = "the registered provider could not be built";
const UNKNOWN_MODEL: &str = "the registered model table has no such model";
const SERVER_FAILED: &str = "the recorded server panicked";
const STREAM_FAILED: &str = "the recorded transcript did not stream";
const HOOK_FAILED: &str = "a provider hook did not answer";
const NO_USAGE: &str = "fetch_usage answered with nothing";
const NO_STATE_DIR: &str = "the isolated state directory did not resolve";
const NO_CREDENTIALS: &str = "the refresh hook stored no credentials";
const TEMPDIR_FAILED: &str = "no temporary state directory";
const BIND_FAILED: &str = "cannot bind loopback";
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

const SSE_HEADERS: &[(&str, &str)] = &[("content-type", "text/event-stream")];
const JSON_HEADERS: &[(&str, &str)] = &[("content-type", "application/json")];
const SLOW_DOWN_HEADERS: &[(&str, &str)] =
    &[("content-type", "application/json"), ("retry-after", "7")];

const CHAT_SCRIPT: &[Canned] = &[Canned::sse(CHAT_TRANSCRIPT)];
const RESPONSES_SCRIPT: &[Canned] = &[Canned::sse(RESPONSES_TRANSCRIPT)];
const NO_REQUESTS: &[Canned] = &[];
const REFRESH_SCRIPT: &[Canned] = &[
    Canned {
        status: 401,
        headers: JSON_HEADERS,
        body: EXPIRED_TOKEN_BODY,
    },
    Canned::sse(CHAT_TRANSCRIPT),
    Canned::sse(CHAT_TRANSCRIPT),
];
const ALLOWANCE_SCRIPT: &[Canned] = &[Canned {
    status: 429,
    headers: SLOW_DOWN_HEADERS,
    body: ALLOWANCE_BODY,
}];
const OVERLOADED_SCRIPT: &[Canned] = &[Canned {
    status: 503,
    headers: SLOW_DOWN_HEADERS,
    body: OVERLOADED_BODY,
}];

/// One recorded response, replayed in script order.
struct Canned {
    status: u16,
    headers: &'static [(&'static str, &'static str)],
    body: &'static str,
}

impl Canned {
    const fn sse(body: &'static str) -> Self {
        Self {
            status: 200,
            headers: SSE_HEADERS,
            body,
        }
    }
}

/// What the plugin actually put on the wire.
struct Recorded {
    headers: HashMap<String, String>,
    body: Value,
}

impl Recorded {
    fn authorization(&self) -> &str {
        self.headers.get("authorization").map_or("", String::as_str)
    }
}

/// Serves `script` in order on loopback, one connection per entry, and hands
/// back every request it saw. Joining before the script is spent would block,
/// so the request count is part of what each test asserts.
fn serve(script: &'static [Canned]) -> (String, JoinHandle<Vec<Recorded>>) {
    let listener = TcpListener::bind(LOOPBACK).expect(BIND_FAILED);
    let base_url = format!("http://{}/v1", listener.local_addr().expect(BIND_FAILED));
    let handle = std::thread::spawn(move || {
        script
            .iter()
            .map(|canned| {
                let (stream, _) = listener.accept().expect(IO_FAILED);
                let recorded = read_request(&stream);
                write_canned(&stream, canned);
                recorded
            })
            .collect()
    });
    (base_url, handle)
}

fn read_request(stream: &TcpStream) -> Recorded {
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut String::new()).expect(IO_FAILED);

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect(IO_FAILED);
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }

    let length = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).expect(IO_FAILED);
    Recorded {
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    }
}

fn write_canned(mut stream: &TcpStream, canned: &Canned) {
    let mut response = format!(
        "HTTP/1.1 {} {REASON_PHRASE}\r\ncontent-length: {}\r\nconnection: close\r\n",
        canned.status,
        canned.body.len()
    );
    for (name, value) in canned.headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(canned.body);
    stream.write_all(response.as_bytes()).expect(IO_FAILED);
    stream.flush().expect(IO_FAILED);
}

/// Points every base directory at a throwaway tree, so the credentials the
/// fixture's `login` and `refresh_auth` store never touch the real state dir.
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

/// The fixture plugin, loaded from disk with the grant its `plugin.toml`
/// declares, talking to a recorded server instead of Acme.
struct Fixture {
    provider: Box<dyn Provider>,
    server: JoinHandle<Vec<Recorded>>,
    _state: TempDir,
    _host: PluginHost,
}

impl Fixture {
    fn start(script: &'static [Canned]) -> Self {
        let state = isolated_state();
        let (base_url, server) = serve(script);
        unsafe { std::env::set_var(BASE_URL_ENV, &base_url) };

        let host = PluginHost::new(Arc::new(ToolRegistry::new())).expect(HOST_FAILED);
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

    fn requests(self) -> Vec<Recorded> {
        self.server.join().expect(SERVER_FAILED)
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
    assert_eq!(sent[0].authorization(), "Bearer anonymous");
    assert_eq!(sent[0].body["model"], json!(MODEL));
    assert_eq!(
        sent[0].body["messages"][0]["content"],
        json!(format!("{SYSTEM_PREFIX}\n\n{SYSTEM}"))
    );
    assert_eq!(
        sent[0].body["acme_reasoning"],
        json!({ "model": MODEL, "effort": SNAPPED_EFFORT, "asked_for": ASKED_EFFORT.as_str() })
    );
    assert!(
        sent[0].body.get("reasoning_effort").is_none(),
        "the hook removed reasoning_effort, so it must not be on the wire: {}",
        sent[0].body
    );
}

/// All three auth entries, each observed through the token that reached the
/// server: `resolve_auth` before the first request, `refresh_auth` after a 401
/// that preceded every event, and `reload_auth` re-reading the store, which by
/// then holds the token the refresh minted and wrote.
#[test]
fn the_auth_hooks_drive_the_credential_lifecycle() {
    let fixture = Fixture::start(REFRESH_SCRIPT);

    fixture.stream(ThinkingConfig::Off).1.expect(STREAM_FAILED);
    smol::block_on(fixture.provider.reload_auth()).expect(HOOK_FAILED);
    fixture.stream(ThinkingConfig::Off).1.expect(STREAM_FAILED);

    let sent = fixture.requests();
    let tokens: Vec<&str> = sent.iter().map(Recorded::authorization).collect();
    assert_eq!(
        tokens,
        [
            "Bearer anonymous",
            "Bearer anonymous-renewed",
            "Bearer anonymous-renewed",
        ]
    );
}

/// The host holds this provider's credential lock while `refresh_auth` runs, so
/// a hook that stores the token it minted has to be let back in through that
/// same lock. It would otherwise wait on its own caller until the hook times
/// out, which is why the budget here is well under `HOOK_TIMEOUT`.
#[test]
fn a_refresh_hook_persists_the_token_it_minted() {
    let fixture = Fixture::start(NO_REQUESTS);

    let started = Instant::now();
    smol::block_on(fixture.provider.refresh_auth()).expect(HOOK_FAILED);
    assert!(
        started.elapsed() < REFRESH_BUDGET,
        "{:?}",
        started.elapsed()
    );

    let dir = StateDir::resolve().expect(NO_STATE_DIR);
    let stored = load_plugin_auth(&dir, SLUG).expect(NO_CREDENTIALS);
    assert_eq!(stored[TOKEN_KEY], json!(RENEWED_TOKEN));
}

/// Both answers come from hooks rather than from the static registration: the
/// tier is one the `models` table never states, and the output window it does
/// state is absent.
#[test]
fn list_models_and_fetch_usage_answer_from_their_hooks() {
    let fixture = Fixture::start(NO_REQUESTS);

    let models = smol::block_on(fixture.provider.list_models()).expect(HOOK_FAILED);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, MODEL);
    assert_eq!(models[0].tier, Some(ModelTier::Strong));
    assert_eq!(models[0].max_output_tokens, None);

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
    assert_eq!(
        error.retry_after(),
        Some(Duration::from_secs(RETRY_AFTER_SECONDS))
    );
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
  build_body = function(body)
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
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).expect(HOST_FAILED);
    host.load_source_with_permissions(
        RESPONSES_SLUG,
        &responses_plugin(&base_url),
        permissions_for(LOOPBACK_HOST),
    )
    .expect(LOAD_FAILED);
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

    let sent = server.join().expect(SERVER_FAILED);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].body["acme_marker"], json!(RESPONSES_MARKER));
}

/// A provider whose `login` parks until the test releases it, so a reload can
/// land while the call is genuinely in flight.
fn parking_plugin(started: &Path, release: &Path) -> String {
    let started = started.display();
    let release = release.display();
    format!(
        r#"
maki.provider.register({{
  slug = "{PARKING_SLUG}",
  display_name = "Acme Parked",
  codec = "openai",
  base_url = "https://{PARKING_HOST}/v1",
  models = {{ {{ prefixes = {{ "{MODEL}" }} }} }},
  login = function()
    maki.fs.write("{started}", "1")
    while not maki.fs.read("{release}") do
      maki.async.sleep({PARKING_POLL_MS})
    end
  end,
}})
"#
    )
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + PARKING_TIMEOUT;
    while !path.exists() {
        assert!(Instant::now() < deadline, "{NEVER_PARKED}");
        std::thread::sleep(Duration::from_millis(PARKING_POLL_MS));
    }
}

/// Two providers in one plugin: one whose `login` writes the credential store
/// while another process holds that slug's lock, and one whose `login` does
/// nothing but answer.
fn locking_plugin(entered: &Path, done: &Path) -> String {
    let entered = entered.display();
    let done = done.display();
    format!(
        r#"
local function provider(slug, login)
  maki.provider.register({{
    slug = slug,
    display_name = slug,
    codec = "openai",
    base_url = "https://{PARKING_HOST}/v1",
    models = {{ {{ prefixes = {{ "{MODEL}" }} }} }},
    login = login,
  }})
end

provider("{LOCKED_SLUG}", function()
  maki.fs.write("{entered}", "1")
  maki.provider.auth.set("{LOCKED_SLUG}", {{ token = "locked" }})
  maki.fs.write("{done}", "1")
end)

provider("{FREE_SLUG}", function() end)
"#
    )
}

/// The defect that keeps the credential store off the plugin host's thread:
/// `lock_credentials` waits on a file lock another maki process may hold, and
/// the host is single threaded, so waiting for it inline stalls every other
/// hook, tool call and timer in the process.
///
/// The lock here is held on a second file descriptor, which is what a second
/// maki process looks like to `flock`, and the in-process re-entrancy that lets
/// a `refresh_auth` persist its own token deliberately does not cover it.
#[test]
fn a_credential_write_does_not_park_the_plugin_host() {
    let state = isolated_state();
    let entered = state.path().join("entered");
    let done = state.path().join("done");

    let host = PluginHost::new(Arc::new(ToolRegistry::new())).expect(HOST_FAILED);
    host.load_source_with_permissions(
        PARKING_PLUGIN,
        &locking_plugin(&entered, &done),
        permissions_for(PARKING_HOST),
    )
    .expect(LOAD_FAILED);
    plugin::commit_load();

    let dir = StateDir::resolve().expect(NO_STATE_DIR);
    let held = lock_plugin_store(&dir, LOCKED_SLUG).expect(NO_TOKEN_LOCK);
    let locked = std::thread::spawn(|| plugin::login(LOCKED_SLUG));
    wait_for(&entered);

    let started = Instant::now();
    plugin::login(FREE_SLUG).expect(HOOK_FAILED);

    assert!(started.elapsed() < UNBLOCKED_BUDGET, "{HOST_THREAD_PARKED}");
    assert!(!done.exists(), "{HOST_THREAD_PARKED}");
    drop(held);
    locked.join().expect(HOOK_THREAD_FAILED).expect(HOOK_FAILED);
}

/// The hook handles outlive the plugin that registered them: an unload drops
/// the plugin's environment, while the registry entries the running call sits
/// on belong to the handle and go only once nobody holds it.
#[test]
fn an_in_flight_hook_call_survives_a_plugin_reload() {
    let state = TempDir::new().expect(TEMPDIR_FAILED);
    let started = state.path().join("started");
    let release = state.path().join("release");

    let host = PluginHost::new(Arc::new(ToolRegistry::new())).expect(HOST_FAILED);
    let load = |source: &str| {
        host.load_source_with_permissions(PARKING_PLUGIN, source, permissions_for(PARKING_HOST))
            .expect(LOAD_FAILED);
    };
    load(&parking_plugin(&started, &release));
    plugin::commit_load();

    let login = std::thread::spawn(|| plugin::login(PARKING_SLUG));
    wait_for(&started);
    load(RELOADED_SOURCE);
    std::fs::write(&release, "1").expect(IO_FAILED);

    login.join().expect(HOOK_THREAD_FAILED).expect(HOOK_FAILED);
}
