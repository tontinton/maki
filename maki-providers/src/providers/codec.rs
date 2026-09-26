use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use flume::Sender;
use isahc::http::header::{CONNECTION, CONTENT_LENGTH, HOST, TRANSFER_ENCODING};
use isahc::http::{HeaderMap, HeaderName, HeaderValue};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value, json};

use maki_config::providers::Protocol;
use maki_storage::id::SessionRef;

use super::openai::responses;
use super::openai_compat::{DEFAULT_MAX_TOKENS_FIELD, OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyRotation, ResolvedAuth, Timeouts};
use crate::model::{Model, ModelInfo, ThinkingSupport};
use crate::model_registry;
use crate::provider::{BoxFuture, Provider};
use crate::spec::{ProviderRegistry, ProviderSpec};
use crate::types::{EffortDialect, ThinkingFallback, dialect, merge_body};
use crate::{AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse};

/// Where effort goes when a declaration names a dialect and no field: the
/// openai chat API's own spelling.
const DEFAULT_EFFORT_FIELD: &str = "reasoning_effort";
const EFFORT_PATH_SEPARATOR: char = '.';
/// Framing the HTTP client owns. A declared one would contradict the request
/// actually sent, or smuggle a second one into it.
const TRANSPORT_HEADERS: [HeaderName; 4] = [HOST, CONTENT_LENGTH, TRANSFER_ENCODING, CONNECTION];

/// Why a declaration's openai wire options did not parse. Raised while the
/// declaration decodes, so a bad one fails the plugin that wrote it rather
/// than a request.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("'{0}' is not a valid header name")]
    HeaderName(String),
    #[error("header '{0}' is framing the HTTP client sets, and cannot be declared")]
    TransportHeader(String),
    #[error("header '{0}' must have a value of visible ASCII")]
    HeaderValue(String),
    #[error("header '{0}' is declared twice")]
    DuplicateHeader(String),
    #[error("effort field '{0}' must be a dotted path of non-empty keys, e.g. `reasoning.effort`")]
    EffortField(String),
    #[error("unknown thinking dialect '{name}' (expected one of {expected})")]
    UnknownDialect { name: String, expected: String },
}

/// What the openai codec lets a declaration say about its wire, as one value
/// that travels unchanged from the Lua table through the declaration and
/// [`CodecOptions`] to [`CompatProvider`]. Nothing copies it field by field,
/// so an option cannot exist in one of those layers and be missing in the
/// next. [`Self::apply_body`] and [`Self::request_headers`] take it apart
/// without `..`, so a field added here fails to compile until it is applied.
#[derive(Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiWire {
    /// `None` is [`DEFAULT_MAX_TOKENS_FIELD`].
    pub max_tokens_field: Option<String>,
    /// `None` asks for `stream_options.include_usage`, which every openai
    /// endpoint worth billing for supports.
    pub include_stream_usage: Option<bool>,
    /// How this provider's API spells reasoning effort. Stating it replaces
    /// the generic thinking pass, see [`Self::apply_body`].
    pub thinking: Option<ThinkingWire>,
    /// Sent with every request, below anything the auth layer set (see
    /// [`OpenAiCompatProvider::do_stream`]).
    #[serde(
        serialize_with = "serialize_headers",
        deserialize_with = "deserialize_headers"
    )]
    pub headers: HeaderMap,
    /// Merged into every request body before the thinking pass.
    pub extra_body: Option<Map<String, Value>>,
    pub session_id: Option<SessionCarrier>,
    /// Model id prefix to what the model can do about thinking, for a
    /// provider whose API knows better than the model table.
    pub thinking_overrides: BTreeMap<String, ThinkingSupport>,
}

/// The effort half of [`OpenAiWire`]. Its own table, because `field` and
/// `requires_support` mean nothing without a dialect to render.
#[derive(Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThinkingWire {
    #[serde(
        serialize_with = "serialize_dialect",
        deserialize_with = "deserialize_dialect"
    )]
    pub dialect: &'static EffortDialect<'static>,
    #[serde(default)]
    pub field: EffortField,
    /// Send effort only to a model that supports thinking, for an API that
    /// rejects the field on any other.
    #[serde(default)]
    pub requires_support: bool,
}

/// Where the rendered effort lands in the body, as the dotted path a
/// declaration writes: `reasoning.effort` nests, `reasoning_effort` does not.
#[derive(Clone, PartialEq, Eq)]
pub struct EffortField(Box<[String]>);

/// Where the session id rides, for a provider that routes a session to the
/// same backend by it. Absent, the id is not sent at all.
#[derive(Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionCarrier {
    Header(
        #[serde(
            serialize_with = "serialize_header_name",
            deserialize_with = "deserialize_header_name"
        )]
        HeaderName,
    ),
    BodyField(String),
}

/// One request, as every pass after the codec's own body sees it. Built once
/// per request, so the thinking pass and the body hook cannot disagree about
/// the model they are looking at.
pub struct RequestCtx<'a> {
    pub model: &'a Model,
    pub opts: RequestOptions,
    pub session: Option<&'a SessionRef>,
    /// What discovery reported for this model under this slug, looked up once
    /// here rather than by each pass that needs it.
    pub discovered: Option<ModelInfo>,
}

impl OpenAiWire {
    /// Everything this declaration adds to a body the codec built. The static
    /// fragment goes in first, so a thinking pass that writes the same key
    /// has the last word, and the session id last of all.
    pub(crate) fn apply_body(&self, body: &mut Value, ctx: &RequestCtx) {
        let Self {
            max_tokens_field: _,
            include_stream_usage: _,
            thinking,
            headers: _,
            extra_body,
            session_id,
            thinking_overrides: _,
        } = self;
        if let (Some(extra), Some(object)) = (extra_body, body.as_object_mut()) {
            merge_body(object, extra);
        }
        // One or the other, never both: a declared dialect is the provider
        // saying how its own API spells effort, a different question from what
        // the model said about itself, so it skips the `thinking_fields` merge.
        match thinking {
            Some(thinking) => thinking.apply(body, ctx),
            None => ctx
                .opts
                .thinking
                .apply_thinking(body, ctx.model, ThinkingFallback::None),
        }
        if let (Some(SessionCarrier::BodyField(field)), Some(session)) = (session_id, ctx.session)
            && let Some(object) = body.as_object_mut()
        {
            object.insert(field.clone(), json!(session.as_str()));
        }
    }

    /// The headers this declaration sends: the static ones, then the session
    /// header when there is a session to carry.
    pub(crate) fn request_headers<'a>(
        &'a self,
        session: Option<&'a SessionRef>,
    ) -> Vec<(&'a str, &'a str)> {
        let Self {
            max_tokens_field: _,
            include_stream_usage: _,
            thinking: _,
            headers,
            extra_body: _,
            session_id,
            thinking_overrides: _,
        } = self;
        let session = match (session_id, session) {
            (Some(SessionCarrier::Header(name)), Some(session)) => {
                Some((name.as_str(), session.as_str()))
            }
            _ => None,
        };
        headers
            .iter()
            .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?)))
            .chain(session)
            .collect()
    }

    /// Applied after everything else that shapes the model, so the provider's
    /// word on thinking is the one that stands.
    pub(crate) fn adjust_model(&self, model: &mut Model) {
        if let Some(support) = self.thinking_override(&model.id) {
            model.thinking_override = Some(support);
        }
    }

    /// The longest matching prefix wins, as it does for a model table row
    /// (see [`crate::model::lookup_entry`]).
    fn thinking_override(&self, model_id: &str) -> Option<ThinkingSupport> {
        self.thinking_overrides
            .iter()
            .filter(|(prefix, _)| model_id.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, support)| *support)
    }
}

impl ThinkingWire {
    /// The declared dialect, narrowed by what discovery listed for the model,
    /// renders the effort: one snap, against the levels the model accepts.
    fn apply(&self, body: &mut Value, ctx: &RequestCtx) {
        if self.requires_support && !ctx.model.supports_thinking() {
            return;
        }
        let dialect = match ctx.discovered.as_ref().and_then(|row| row.effort.as_ref()) {
            Some(effort) => effort.refine(self.dialect),
            None => self.dialect.clone(),
        };
        if let Some(effort) = ctx.opts.thinking.effort_str(&dialect, ctx.model)
            && let Some(object) = body.as_object_mut()
        {
            self.field.write(object, effort);
        }
    }
}

impl EffortField {
    pub(crate) fn parse(path: &str) -> Result<Self, WireError> {
        let keys: Box<[String]> = path
            .split(EFFORT_PATH_SEPARATOR)
            .map(str::to_owned)
            .collect();
        if keys.iter().any(String::is_empty) {
            return Err(WireError::EffortField(path.to_owned()));
        }
        Ok(Self(keys))
    }

    /// Merged rather than assigned, so a sibling the body already holds under
    /// the same parent survives.
    fn write(&self, body: &mut Map<String, Value>, effort: &str) {
        let Some((outermost, nested)) = self.0.split_first() else {
            return;
        };
        let value = nested.iter().rev().fold(Value::from(effort), |value, key| {
            Value::Object(Map::from_iter([(key.clone(), value)]))
        });
        merge_body(body, &Map::from_iter([(outermost.clone(), value)]));
    }
}

impl Default for EffortField {
    fn default() -> Self {
        Self(Box::new([DEFAULT_EFFORT_FIELD.to_owned()]))
    }
}

impl Serialize for EffortField {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.join(&EFFORT_PATH_SEPARATOR.to_string()))
    }
}

impl<'de> Deserialize<'de> for EffortField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Dumped as the dialect's name, so the table stays the one source of truth
/// for what a dialect is and a declaration never carries the name twice.
fn serialize_dialect<S: Serializer>(
    dialect: &&'static EffortDialect<'static>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    dialect::name_of(dialect).serialize(serializer)
}

fn deserialize_dialect<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<&'static EffortDialect<'static>, D::Error> {
    let name = String::deserialize(deserializer)?;
    dialect::by_name(&name).ok_or_else(|| {
        D::Error::custom(WireError::UnknownDialect {
            expected: dialect::NAMES.join(", "),
            name,
        })
    })
}

fn declared_header_name(name: &str) -> Result<HeaderName, WireError> {
    let parsed = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| WireError::HeaderName(name.to_owned()))?;
    if TRANSPORT_HEADERS.contains(&parsed) {
        return Err(WireError::TransportHeader(name.to_owned()));
    }
    Ok(parsed)
}

/// Visible ASCII only, so every value [`OpenAiWire::request_headers`] hands on
/// renders as text.
fn declared_headers(declared: BTreeMap<String, String>) -> Result<HeaderMap, WireError> {
    let mut headers = HeaderMap::with_capacity(declared.len());
    for (name, value) in declared {
        let parsed = declared_header_name(&name)?;
        let value = HeaderValue::from_str(&value)
            .ok()
            .filter(|value| value.to_str().is_ok())
            .ok_or_else(|| WireError::HeaderValue(name.clone()))?;
        if headers.insert(parsed, value).is_some() {
            return Err(WireError::DuplicateHeader(name));
        }
    }
    Ok(headers)
}

fn serialize_headers<S: Serializer>(headers: &HeaderMap, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_map(
        headers
            .iter()
            .map(|(name, value)| (name.as_str(), String::from_utf8_lossy(value.as_bytes()))),
    )
}

fn deserialize_headers<'de, D: Deserializer<'de>>(deserializer: D) -> Result<HeaderMap, D::Error> {
    declared_headers(BTreeMap::deserialize(deserializer)?).map_err(D::Error::custom)
}

fn serialize_header_name<S: Serializer>(
    name: &HeaderName,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(name.as_str())
}

fn deserialize_header_name<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<HeaderName, D::Error> {
    declared_header_name(&String::deserialize(deserializer)?).map_err(D::Error::custom)
}

/// Everything a provider declaration tells the codec about its wire: what an
/// [`OpenAiCompatConfig`] is made of, plus what a declaration adds on top of
/// the protocol. Passed whole so a new option is one more field here rather
/// than one more argument at every call site.
///
/// `Cow` throughout because the fields land in an [`OpenAiCompatConfig`],
/// which the native providers fill with `&'static str`.
pub struct CodecOptions {
    pub protocol: Protocol,
    /// Never empty: it is the key
    /// [`maki_config::providers::configured_base_url`] looks the user's
    /// `<SLUG>_BASE_URL` and `providers.toml` origin up under.
    pub slug: Cow<'static, str>,
    pub api_key_env: Cow<'static, str>,
    /// The origin the declaration itself names, and the weakest of the three
    /// (see [`OpenAiCompatProvider::base_url`]): the user's env var and
    /// `providers.toml` beat it, and an origin a hook resolved beats those.
    pub base_url: Cow<'static, str>,
    /// A log label, the slug by default.
    pub provider_name: Cow<'static, str>,
    pub system_prefix: Option<String>,
    /// Only the `openai` codec reads it, and registration refuses one stated
    /// for any other.
    pub openai: OpenAiWire,
    pub build_body: Option<Arc<dyn BodyHook>>,
}

impl CodecOptions {
    /// Everything but the protocol and the slug behaves as a plain openai
    /// endpoint, so a caller states only what it differs on.
    pub fn new(protocol: Protocol, slug: impl Into<Cow<'static, str>>) -> Self {
        let slug = slug.into();
        Self {
            protocol,
            api_key_env: Cow::Borrowed(""),
            base_url: Cow::Borrowed(""),
            provider_name: slug.clone(),
            slug,
            system_prefix: None,
            openai: OpenAiWire::default(),
            build_body: None,
        }
    }
}

/// The native provider a custom or plugin slug borrows its codec and fallbacks
/// from. Resolved through [`ProviderRegistry::get`], never `for_slug`, so the
/// lookup cannot recurse back into here.
pub(crate) fn protocol_spec(protocol: Protocol) -> Option<&'static ProviderSpec> {
    ProviderRegistry::get(match protocol {
        Protocol::Openai | Protocol::OpenaiResponses => super::openai::SLUG,
        Protocol::Anthropic => super::anthropic::SLUG,
        Protocol::Google => super::google::SLUG,
    })
}

/// Applied to the final request body, after the codec built it and after the
/// thinking pass, so a hook sees exactly what goes on the wire.
pub trait BodyHook: Send + Sync {
    fn call<'a>(
        &'a self,
        body: Value,
        ctx: &RequestCtx<'_>,
    ) -> BoxFuture<'a, Result<Value, AgentError>>;
}

/// Where a declaration's wire options become the compat layer's, so
/// `max_tokens_field` and `include_stream_usage` pick up their defaults in one
/// place.
fn compat_config(options: &CodecOptions) -> OpenAiCompatConfig {
    OpenAiCompatConfig {
        slug: options.slug.clone(),
        api_key_env: options.api_key_env.clone(),
        base_url: options.base_url.clone(),
        max_tokens_field: options
            .openai
            .max_tokens_field
            .clone()
            .map_or(Cow::Borrowed(DEFAULT_MAX_TOKENS_FIELD), Cow::Owned),
        include_stream_usage: options.openai.include_stream_usage.unwrap_or(true),
        provider_name: options.provider_name.clone(),
    }
}

/// The one place a protocol picks its codec. Every codec here honours
/// `system_prefix` except google, which drops it (see [`super::google`]) and
/// refuses one at registration instead.
pub fn build(
    options: CodecOptions,
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
) -> Box<dyn Provider> {
    match options.protocol {
        Protocol::Anthropic => Box::new(
            super::anthropic::Anthropic::with_auth(auth, timeouts)
                .with_system_prefix(options.system_prefix),
        ),
        Protocol::Openai | Protocol::OpenaiResponses => Box::new(CompatProvider {
            compat: OpenAiCompatProvider::new(compat_config(&options), timeouts),
            auth,
            protocol: options.protocol,
            system_prefix: options.system_prefix,
            openai: options.openai,
            build_body: options.build_body,
        }),
        Protocol::Google => Box::new(super::google::Google::with_auth(auth, timeouts)),
    }
}

pub(crate) struct CompatProvider {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    protocol: Protocol,
    system_prefix: Option<String>,
    openai: OpenAiWire,
    build_body: Option<Arc<dyn BodyHook>>,
}

#[warn(clippy::missing_trait_methods)]
impl Provider for CompatProvider {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let mut auth = self.auth.lock().unwrap().clone();
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);
            let ctx = RequestCtx {
                model,
                opts,
                session: session_id,
                discovered: model_registry::discovered(&self.compat.config().slug, &model.id),
            };

            if self.protocol == Protocol::OpenaiResponses {
                // `responses::do_stream` reads the origin off the auth alone,
                // so the three-way precedence has to be settled here. Nothing
                // resolved means nothing written, so a declaration with no
                // origin anywhere still fails there rather than posting to "".
                let resolved = self.compat.base_url(&auth);
                if !resolved.is_empty() {
                    auth.base_url = Some(resolved);
                }
                let mut body = responses::build_body(model, messages, system, tools);
                // TODO: wire thinking budget into responses API when llama.cpp supports it
                if let Some(hook) = &self.build_body {
                    body = hook.call(body, &ctx).await?;
                }
                return responses::do_stream(
                    self.compat.client(),
                    model,
                    &body,
                    event_tx,
                    &auth,
                    self.compat.stream_timeout(),
                )
                .await;
            }

            let mut body = self.compat.build_body(model, messages, system, tools);
            self.openai.apply_body(&mut body, &ctx);
            if let Some(hook) = &self.build_body {
                body = hook.call(body, &ctx).await?;
            }
            let headers = self.openai.request_headers(session_id);
            self.compat
                .do_stream(model, &headers, &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        let auth = self.auth.lock().unwrap().clone();
        Box::pin(async move { self.compat.do_list_models(&auth).await })
    }

    /// The protocol has no usage endpoint. A declaration whose provider has
    /// one answers through its `fetch_usage` hook, a layer above this.
    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async { Ok(None) })
    }

    /// Credentials are minted above the codec, which only reads the shared
    /// cell per request, so there is nothing here to refresh.
    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async { Ok(()) })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async { Ok(()) })
    }

    /// The key pool belongs to whoever resolved the credentials, which wraps
    /// this provider and answers for it.
    fn keys(&self) -> Option<KeyRotation<'_>> {
        None
    }

    fn adjust_model(&self, model: &mut Model) {
        self.openai.adjust_model(model);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use test_case::test_case;

    use super::super::plugin::PluginAuth;
    use super::*;
    use crate::model::ModelEffort;
    use crate::{Effort, ThinkingConfig};

    const TITLE_HEADER: &str = "X-Title";
    const AFFINITY_HEADER: &str = "x-affinity";
    const SAFE_VALUE: &str = "maki";
    const SESSION_FIELD: &str = "session_id";
    const MODEL_SPEC: &str = "synthetic/hf:moonshotai/Kimi-K2.5";

    /// One slug per case: `<SLUG>_BASE_URL` is process-wide, and these run in
    /// one process under `cargo test`.
    const DECLARED_SLUG: &str = "codec-declared-origin";
    const OVERRIDDEN_SLUG: &str = "codec-overridden-origin";
    const OVERRIDDEN_ENV: &str = "CODEC_OVERRIDDEN_ORIGIN_BASE_URL";
    const HOOKED_SLUG: &str = "codec-hooked-origin";
    const HOOKED_ENV: &str = "CODEC_HOOKED_ORIGIN_BASE_URL";
    const DECLARED_URL: &str = "https://declared.example/v1";
    const USER_URL: &str = "https://user.example/v1";
    const HOOK_URL: &str = "https://hooked.example/v1";
    const HOOK_HOST: &str = "hooked.example";

    /// A declaration that states an origin and nothing else.
    fn compat(slug: &'static str) -> OpenAiCompatProvider {
        let options = CodecOptions {
            base_url: Cow::Borrowed(DECLARED_URL),
            ..CodecOptions::new(Protocol::Openai, slug)
        };
        OpenAiCompatProvider::new(compat_config(&options), Timeouts::default())
    }

    fn no_credentials(slug: &str) -> ResolvedAuth {
        ResolvedAuth::new(slug, Vec::new()).unwrap()
    }

    #[test]
    fn a_declared_origin_is_the_last_resort() {
        let compat = compat(DECLARED_SLUG);
        assert_eq!(
            compat.base_url(&no_credentials(DECLARED_SLUG)),
            DECLARED_URL
        );
    }

    /// The bug the empty `slug` on the old shared config hid: a plugin or
    /// `providers.toml` provider never looked at `<SLUG>_BASE_URL` at all, so
    /// a declaration outranked the user.
    #[test]
    fn a_user_origin_beats_a_declared_one() {
        unsafe { std::env::set_var(OVERRIDDEN_ENV, USER_URL) };
        let compat = compat(OVERRIDDEN_SLUG);
        assert_eq!(compat.base_url(&no_credentials(OVERRIDDEN_SLUG)), USER_URL);
    }

    /// The two origins live in different fields, so one cannot overwrite the
    /// other: a hook's lands in the auth cell through
    /// [`PluginAuth::into_resolved`], the one door that vets it against the
    /// declared hosts, while the user's sits in `resolved_base_url` where no
    /// hook can reach it.
    #[test]
    fn a_hook_origin_wins_only_for_the_request_it_answered() {
        unsafe { std::env::set_var(HOOKED_ENV, USER_URL) };
        let compat = compat(HOOKED_SLUG);
        let hooked = PluginAuth {
            base_url: Some(HOOK_URL.to_string()),
            headers: HashMap::new(),
        }
        .into_resolved(HOOKED_SLUG, &[HOOK_HOST.to_string()])
        .unwrap();

        assert_eq!(compat.base_url(&hooked), HOOK_URL);
        assert_eq!(compat.base_url(&no_credentials(HOOKED_SLUG)), USER_URL);
    }

    fn wire(authored: Value) -> Result<OpenAiWire, String> {
        serde_json::from_value(authored).map_err(|e| e.to_string())
    }

    fn synthetic_model() -> Model {
        Model::from_spec(MODEL_SPEC).unwrap()
    }

    fn ctx(model: &Model, thinking: ThinkingConfig) -> RequestCtx<'_> {
        RequestCtx {
            model,
            opts: RequestOptions {
                thinking,
                ..RequestOptions::default()
            },
            session: None,
            discovered: None,
        }
    }

    /// Header names and values are parsed when the declaration decodes, so a
    /// header that could not go on the wire, or that would contradict the
    /// framing the client writes, fails the plugin and never a request.
    #[test_case("Host", SAFE_VALUE, WireError::TransportHeader("Host".into()) ; "host")]
    #[test_case("content-length", SAFE_VALUE, WireError::TransportHeader("content-length".into()) ; "content_length")]
    #[test_case("Transfer-Encoding", SAFE_VALUE, WireError::TransportHeader("Transfer-Encoding".into()) ; "transfer_encoding")]
    #[test_case("connection", SAFE_VALUE, WireError::TransportHeader("connection".into()) ; "connection")]
    #[test_case("bad header", SAFE_VALUE, WireError::HeaderName("bad header".into()) ; "space_in_the_name")]
    #[test_case(TITLE_HEADER, "caf\u{e9}", WireError::HeaderValue(TITLE_HEADER.into()) ; "non_ascii_value")]
    fn a_header_the_wire_cannot_carry_is_refused(name: &str, value: &str, expected: WireError) {
        let error = wire(json!({ "headers": { name: value } }))
            .err()
            .unwrap_or_default();
        assert!(error.contains(&expected.to_string()), "{error}");
    }

    #[test_case("" ; "empty")]
    #[test_case("reasoning..effort" ; "empty_middle_key")]
    #[test_case(".effort" ; "leading_separator")]
    fn an_effort_path_with_an_empty_key_is_refused(field: &str) {
        let authored = json!({ "thinking": { "dialect": "standard", "field": field } });
        let error = wire(authored).err().unwrap_or_default();
        assert!(
            error.contains(&WireError::EffortField(field.into()).to_string()),
            "{error}"
        );
    }

    /// The static headers go out under their canonical names, and the session
    /// header after them, only when there is a session to carry.
    #[test]
    fn declared_headers_and_the_session_header_are_sent() {
        let wire = wire(json!({
            "headers": { TITLE_HEADER: SAFE_VALUE },
            "session_id": { "header": AFFINITY_HEADER },
        }))
        .unwrap();
        let session = SessionRef::generate();

        assert_eq!(
            wire.request_headers(None),
            [(TITLE_HEADER.to_ascii_lowercase().as_str(), SAFE_VALUE)]
        );
        assert_eq!(
            wire.request_headers(Some(&session)),
            [
                (TITLE_HEADER.to_ascii_lowercase().as_str(), SAFE_VALUE),
                (AFFINITY_HEADER, session.as_str()),
            ]
        );
    }

    /// The static fragment, the effort under a nested path that keeps its
    /// siblings, and the session id in the body.
    #[test]
    fn the_declared_body_options_reach_the_body() {
        let wire = wire(json!({
            "thinking": { "dialect": "standard", "field": "reasoning.effort" },
            "extra_body": { "reasoning": { "exclude": true }, "cache_control": { "type": "ephemeral" } },
            "session_id": { "body_field": SESSION_FIELD },
        }))
        .unwrap();
        let model = synthetic_model();
        let session = SessionRef::generate();
        let ctx = RequestCtx {
            session: Some(&session),
            ..ctx(&model, ThinkingConfig::Effort(Effort::High))
        };
        let mut body = json!({ "model": model.id });

        wire.apply_body(&mut body, &ctx);

        assert_eq!(
            body,
            json!({
                "model": model.id,
                "reasoning": { "exclude": true, "effort": "high" },
                "cache_control": { "type": "ephemeral" },
                SESSION_FIELD: session.as_str(),
            })
        );
    }

    #[test_case(false, true ; "sent_to_any_model_by_default")]
    #[test_case(true, false ; "withheld_from_a_model_without_thinking")]
    fn requires_support_gates_the_effort(requires_support: bool, sent: bool) {
        let wire = wire(json!({
            "thinking": { "dialect": "standard", "requires_support": requires_support },
        }))
        .unwrap();
        let mut model = synthetic_model();
        model.thinking_override = Some(ThinkingSupport::No);
        let mut body = json!({});

        wire.apply_body(
            &mut body,
            &ctx(&model, ThinkingConfig::Effort(Effort::High)),
        );

        assert_eq!(body.get(DEFAULT_EFFORT_FIELD).is_some(), sent, "{body}");
    }

    /// A listed model's levels replace the declared ones before the one snap,
    /// so a level past the declared ceiling still goes out.
    #[test_case(Some(vec![Effort::High, Effort::XHigh]), "xhigh" ; "listed_levels_refine_the_dialect")]
    #[test_case(None, "high" ; "unlisted_model_keeps_the_declared_dialect")]
    fn a_discovered_effort_refines_the_declared_dialect(
        listed: Option<Vec<Effort>>,
        expected: &str,
    ) {
        let wire = wire(json!({ "thinking": { "dialect": "prefer-high" } })).unwrap();
        let model = synthetic_model();
        let ctx = RequestCtx {
            discovered: Some(ModelInfo {
                effort: listed.map(|supported| ModelEffort {
                    supported,
                    send_off: None,
                }),
                ..ModelInfo::id_only(model.id.clone())
            }),
            ..ctx(&model, ThinkingConfig::Effort(Effort::Max))
        };
        let mut body = json!({});

        wire.apply_body(&mut body, &ctx);

        assert_eq!(body[DEFAULT_EFFORT_FIELD], expected);
    }

    #[test_case("ministral-8b-latest", Some(ThinkingSupport::No) ; "shorter_prefix")]
    #[test_case("ministral-large-2", Some(ThinkingSupport::Yes) ; "longest_prefix_wins")]
    #[test_case("mistral-medium-latest", None ; "no_prefix_leaves_the_model_alone")]
    fn thinking_overrides_take_the_longest_prefix(
        model_id: &str,
        expected: Option<ThinkingSupport>,
    ) {
        let wire = wire(json!({
            "thinking_overrides": { "ministral-": "no", "ministral-large": "yes" },
        }))
        .unwrap();
        assert_eq!(wire.thinking_override(model_id), expected);
    }
}
