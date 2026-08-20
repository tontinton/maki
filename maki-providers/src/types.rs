//! Message and content types for provider communication.
//! `Message.display_text`: `Some("")` marks a message as synthetic (sent to the API but hidden
//! from the UI). `user_text()` returns `None` for these, so system-injected messages
//! (cancel markers, compaction prompts) stay invisible without a separate type.
//! `Message.kind` answers a different question. Synthetic text is ours and
//! trusted, it is just not worth showing. An observation comes from outside,
//! belongs in model context, and must never be mistaken for the user talking.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

pub use maki_storage::sessions::Effort;
use maki_storage::sessions::{MIN_THINKING_BUDGET, StoredThinking, TitleSource};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use strum::{Display, IntoStaticStr};
use tracing::warn;

use crate::TokenUsage;
use crate::model::Model;

const LOCAL_BUDGET_FIELD: &str = "thinking_budget_tokens";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageMediaType {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageMediaType {
    pub const ALL: [Self; 4] = [Self::Png, Self::Jpeg, Self::Gif, Self::Webp];

    /// Single source of truth for media-type strings: serde, data URLs,
    /// wire formats, and the Lua bridge all go through here.
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    pub fn from_mime(mime: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.mime() == mime)
    }
}

impl Serialize for ImageMediaType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.mime())
    }
}

impl<'de> Deserialize<'de> for ImageMediaType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::from_mime(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown image media type '{s}'")))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ImageSource {
    pub media_type: ImageMediaType,
    pub data: Arc<str>,
}

impl Serialize for ImageSource {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ImageSource", 3)?;
        state.serialize_field("type", "base64")?;
        state.serialize_field("media_type", &self.media_type)?;
        state.serialize_field("data", &self.data)?;
        state.end()
    }
}

impl ImageSource {
    pub fn new(media_type: ImageMediaType, data: Arc<str>) -> Self {
        Self { media_type, data }
    }

    pub fn to_data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type.mime(), self.data)
    }
}

pub const IMAGE_OMITTED_NOTE: &str =
    "[image omitted: the current model does not support image input]";
/// See [`Message::empty_marker`].
pub const EMPTY_RESPONSE_MARKER: &str = "(empty)";

/// For models without vision, image blocks become a text note instead of a
/// wire block the API would reject. History keeps the pixels, so switching
/// back to a vision-capable model restores them.
pub fn adapt_images_for_model<'a>(model: &Model, messages: &'a [Message]) -> Cow<'a, [Message]> {
    let has_image = |m: &Message| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Image { .. }))
    };
    if model.supports_vision() || !messages.iter().any(has_image) {
        return Cow::Borrowed(messages);
    }
    let adapted = messages
        .iter()
        .map(|m| {
            let mut m = m.clone();
            for block in &mut m.content {
                if matches!(block, ContentBlock::Image { .. }) {
                    *block = ContentBlock::Text {
                        text: IMAGE_OMITTED_NOTE.into(),
                    };
                }
            }
            m
        })
        .collect();
    Cow::Owned(adapted)
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    User,
    Assistant,
}

impl Role {
    fn is_user(&self) -> bool {
        matches!(self, Self::User)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    Image {
        source: ImageSource,
    },
}

impl ContentBlock {
    pub fn is_thinking(&self) -> bool {
        matches!(self, Self::Thinking { .. } | Self::RedactedThinking { .. })
    }
}

/// Who a message came from, which `role` cannot say. Providers only
/// accept user and assistant, so anything the host wants to report has to
/// travel as a user message, and without this there is no way to tell it
/// apart from the user actually typing. A prefix in the text would not do:
/// a log line can print one.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// Someone said this, the user or the model.
    #[default]
    Turn,
    /// The host noticed it and passed it to the model. It stays in session
    /// history for conversation order but is hidden from user-facing views.
    Observation,
}

impl MessageKind {
    fn is_turn(&self) -> bool {
        matches!(self, Self::Turn)
    }
}

impl ContentBlock {
    pub fn tool_use(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        Self::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
            thought_signature: None,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_text: Option<String>,
    /// Skipped when it is `Turn`, so sessions written before this existed
    /// load unchanged.
    #[serde(default, skip_serializing_if = "MessageKind::is_turn")]
    pub kind: MessageKind,
}

impl Message {
    /// Stands in for an assistant turn with no text, thinking alone or empty,
    /// which providers reject as the trailing message. Never a real response:
    /// readers mining history for model text must skip it.
    pub fn empty_marker() -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: EMPTY_RESPONSE_MARKER.into(),
            }],
            ..Default::default()
        }
    }

    /// Something the host saw, reported to the model without pretending
    /// the user said it.
    pub fn observation(text: String) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text }],
            kind: MessageKind::Observation,
            ..Default::default()
        }
    }

    pub fn is_observation(&self) -> bool {
        self.kind == MessageKind::Observation
    }

    pub fn user(text: String) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text }],
            ..Default::default()
        }
    }

    pub fn user_display(ai_text: String, display: String) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: ai_text }],
            display_text: Some(display),
            ..Default::default()
        }
    }

    pub fn user_with_images(text: String, images: Vec<ImageSource>) -> Self {
        let mut content: Vec<ContentBlock> = images
            .into_iter()
            .map(|source| ContentBlock::Image { source })
            .collect();
        if !text.is_empty() {
            content.push(ContentBlock::Text { text });
        }
        Self {
            role: Role::User,
            content,
            ..Default::default()
        }
    }

    pub fn synthetic(text: String) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text }],
            display_text: Some(String::new()),
            ..Default::default()
        }
    }

    pub fn user_text(&self) -> Option<&str> {
        match &self.display_text {
            Some(t) if t.is_empty() => None,
            Some(t) => Some(t),
            None => self.first_text_content(),
        }
    }

    pub fn first_text_content(&self) -> Option<&str> {
        self.content.iter().find_map(|b| match b {
            ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
    }

    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolUse {
                id, name, input, ..
            } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }

    pub fn has_tool_calls(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
    }
}

impl TitleSource for Message {
    fn first_user_text(&self) -> Option<&str> {
        if !self.role.is_user() || self.is_observation() {
            return None;
        }
        self.user_text()
    }
}

#[derive(Debug, Clone, Serialize)]
pub enum ProviderEvent {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolUseStart {
        id: String,
        name: String,
    },
    PromptProgress {
        processed: u32,
        total: u32,
        cache: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Display, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

impl StopReason {
    pub fn from_anthropic(s: &str) -> Self {
        match s {
            "end_turn" => Self::EndTurn,
            "tool_use" => Self::ToolUse,
            "max_tokens" => Self::MaxTokens,
            _ => Self::EndTurn,
        }
    }

    pub fn from_openai(s: &str) -> Self {
        match s {
            "stop" => Self::EndTurn,
            "tool_calls" => Self::ToolUse,
            "length" => Self::MaxTokens,
            _ => Self::EndTurn,
        }
    }

    pub fn from_google(s: &str) -> Self {
        match s {
            "STOP" => Self::EndTurn,
            "MAX_TOKENS" => Self::MaxTokens,
            "SAFETY" | "RECITATION" => {
                warn!("Gemini stop reason: {s}, treating as end_turn");
                Self::EndTurn
            }
            _ => Self::EndTurn,
        }
    }
}

const THINKING_USAGE: &str =
    "Usage: /thinking [off|adaptive|minimal|low|medium|high|xhigh|max|<budget>]";

/// Effort levels are percentages, so they need a ceiling even when the model
/// never told us its output window. 32k matches common frontier thinking
/// caps. Explicit user budgets never go through this.
const FALLBACK_MAX_THINKING_BUDGET: u32 = 32_768;

/// First Claude version that speaks adaptive thinking. Opus got there a
/// generation early, at 4.7; the other families joined at 5.
const ADAPTIVE_SINCE: (u32, u32) = (5, 0);
const ADAPTIVE_SINCE_OPUS: (u32, u32) = (4, 7);
const OPUS: &str = "opus";

/// `claude-opus-4.7` -> `("opus", (4, 7))`, `claude-opus-5-1m` -> `("opus", (5, 0))`.
/// Copilot writes the version with a dot, hence the two separators. Legacy ids
/// put the version first (`claude-3-5-sonnet-20241022`), so a numeric family
/// tells us there is no modern version to read here. Gateway ids keep a
/// vendor prefix (`anthropic/claude-opus-4-7`), so read the last path segment.
fn claude_version(model_id: &str) -> Option<(&str, (u32, u32))> {
    let bare = model_id.rsplit('/').next().unwrap_or(model_id);
    let mut parts = bare.strip_prefix("claude-")?.split(['-', '.']);
    let family = parts.next().filter(|f| f.parse::<u32>().is_err())?;
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((family, (major, minor)))
}

/// How a provider's effort knob speaks: which levels its API accepts, what
/// `adaptive` means there, and whether "off" needs an explicit string.
/// New providers add a const in [`dialect`]; providers with dynamic model
/// listings build one from the model's declared levels (see OpenRouter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortDialect<'a> {
    /// Accepted levels, non-empty and ascending (checked by test).
    pub supported: &'a [Effort],
    /// What `Adaptive` maps to. `None` means the API has its own adaptive or
    /// default behavior: send nothing and let it decide.
    pub adaptive: Option<Effort>,
    /// Explicit opt-out string, e.g. GLM `"none"`.
    pub off: Option<&'static str>,
}

/// How a local model spells thinking on the wire, in place of a token budget.
/// Each mode carries the JSON fragment merged into the request body, so any
/// shape a chat template needs works without a schema per provider.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ThinkingFields {
    #[serde(default)]
    off: Option<Map<String, Value>>,
    #[serde(default)]
    adaptive: Option<Map<String, Value>>,
    /// Keyed by [`Effort`]; the declared keys are the levels the model accepts.
    #[serde(flatten)]
    levels: BTreeMap<Effort, Map<String, Value>>,
}

impl ThinkingFields {
    /// Levels snap to the declared ones, so a level the model never advertised
    /// is never sent. A token budget picks the level it corresponds to; models
    /// that declare no levels fall back to `adaptive` and keep the count
    /// (the returned flag tells the caller to still send the budget field).
    fn fragment(
        &self,
        thinking: ThinkingConfig,
        max: Option<u32>,
    ) -> Option<(&Map<String, Value>, bool)> {
        let level = match thinking {
            ThinkingConfig::Off => return self.off.as_ref().map(|f| (f, false)),
            ThinkingConfig::Adaptive => return self.adaptive.as_ref().map(|f| (f, false)),
            ThinkingConfig::Effort(level) => level,
            ThinkingConfig::Budget(n) => {
                if self.levels.is_empty() {
                    return self.adaptive.as_ref().map(|f| (f, true));
                }
                Effort::from_budget(n, max.unwrap_or(FALLBACK_MAX_THINKING_BUDGET))
            }
        };
        let declared: Vec<Effort> = self.levels.keys().copied().collect();
        self.levels
            .get(&level.snap(&declared))
            .or(self.adaptive.as_ref())
            .map(|f| (f, false))
    }
}

fn merge_body(body: &mut Map<String, Value>, fragment: &Map<String, Value>) {
    for (key, value) in fragment {
        match (body.get_mut(key), value.as_object()) {
            (Some(Value::Object(target)), Some(source)) => merge_body(target, source),
            _ => {
                body.insert(key.clone(), value.clone());
            }
        }
    }
}

pub mod dialect {
    use super::EffortDialect;
    use maki_storage::sessions::Effort::{High, Low, Max, Medium, Minimal, XHigh};

    /// Wire string that disables reasoning, for APIs that need an explicit
    /// opt-out.
    pub const OFF: &str = "none";

    /// OpenAI platform, synthetic.
    pub const STANDARD: EffortDialect = EffortDialect {
        supported: &[Minimal, Low, Medium, High],
        adaptive: Some(Medium),
        off: None,
    };
    /// opencode chat-completions, openrouter (static fallback).
    pub const PREFER_HIGH: EffortDialect = EffortDialect {
        supported: &[Low, Medium, High],
        adaptive: Some(High),
        off: None,
    };
    /// Mistral.
    pub const HIGH_ONLY: EffortDialect = EffortDialect {
        supported: &[High],
        adaptive: Some(High),
        off: None,
    };
    /// Z.AI. GLM reasons by default, so Off sends "none" explicitly.
    /// Only use behind `Model::supports_thinking`.
    pub const GLM: EffortDialect = EffortDialect {
        supported: &[High, XHigh],
        adaptive: Some(High),
        off: Some(OFF),
    };
    /// DeepSeek accepts only "max"; Adaptive keeps the model's own default
    /// reasoning depth by sending no effort at all.
    pub const DEEPSEEK: EffortDialect = EffortDialect {
        supported: &[Max],
        adaptive: None,
        off: None,
    };
    /// `output_config.effort` on Anthropic adaptive-thinking models. The API
    /// has native adaptive mode, so Adaptive sends no effort.
    pub const ANTHROPIC_ADAPTIVE: EffortDialect = EffortDialect {
        supported: &[Low, Medium, High],
        adaptive: None,
        off: None,
    };
    /// TensorX routes models that may reason by default, so Off sends "none"
    /// explicitly and Adaptive asks for full depth.
    pub const TENSORX: EffortDialect = EffortDialect {
        supported: &[Low, Medium, High],
        adaptive: Some(High),
        off: Some(OFF),
    };
    /// xAI Grok 4.5/4.6. Adaptive defaults to high; Off sends nothing so the
    /// model keeps its own default. `xhigh` is advertised on Grok 4.6.
    pub const GROK: EffortDialect = EffortDialect {
        supported: &[Low, Medium, High, XHigh],
        adaptive: Some(High),
        off: None,
    };
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThinkingConfig {
    #[default]
    Off,
    Adaptive,
    Effort(Effort),
    Budget(u32),
}

/// Resolved thinking value for token-budget APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Budgeted {
    Off,
    Adaptive,
    Tokens(u32),
}

impl ThinkingConfig {
    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// The effort string to send, snapped to the dialect's supported levels
    /// here and nowhere else (never chain snaps). `None` means send nothing:
    /// `Off` without an explicit off string, or `Adaptive` on APIs with their
    /// own default behavior.
    pub fn effort_str(self, dialect: &EffortDialect, model: &Model) -> Option<&'static str> {
        let level = match self {
            Self::Off => return dialect.off,
            Self::Adaptive => dialect.adaptive?,
            Self::Effort(e) => e,
            Self::Budget(n) => Effort::from_budget(
                n,
                model
                    .max_thinking_budget()
                    .unwrap_or(FALLBACK_MAX_THINKING_BUDGET),
            ),
        };
        Some(level.snap(dialect.supported).as_str())
    }

    /// The token budget to send, clamped to `[MIN_THINKING_BUDGET, max]` here
    /// and nowhere else. An unknown `max` never caps: the user's number goes
    /// through as asked, and effort levels scale the fallback ceiling.
    fn budget(self, max: Option<u32>) -> Budgeted {
        match self {
            Self::Off => Budgeted::Off,
            Self::Adaptive => Budgeted::Adaptive,
            Self::Effort(e) => {
                Budgeted::Tokens(e.budget(max.unwrap_or(FALLBACK_MAX_THINKING_BUDGET)))
            }
            Self::Budget(n) => Budgeted::Tokens(match max {
                Some(max) => n.clamp(MIN_THINKING_BUDGET, max.max(MIN_THINKING_BUDGET)),
                None => n.max(MIN_THINKING_BUDGET),
            }),
        }
    }

    /// Anthropic messages API body. Adaptive-thinking models get the native
    /// adaptive knob plus `output_config.effort`; legacy models get a plain
    /// token budget.
    pub fn apply_to_body(self, body: &mut Value, model: &Model) {
        if Self::requires_adaptive(&model.id) {
            if matches!(self, Self::Off) {
                return;
            }
            // These models default `display` to "omitted", so thinking arrives
            // empty and tool calls pop up out of nowhere in the UI. Asking for
            // the summary back costs nothing: thinking tokens bill the same.
            body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
            if let Some(effort) = self.effort_str(&dialect::ANTHROPIC_ADAPTIVE, model) {
                body["output_config"]["effort"] = json!(effort);
            }
            return;
        }
        match self.budget(model.max_thinking_budget()) {
            Budgeted::Off => {}
            Budgeted::Adaptive => body["thinking"] = json!({"type": "adaptive"}),
            Budgeted::Tokens(n) => {
                body["thinking"] = json!({"type": "enabled", "budget_tokens": n});
            }
        }
    }

    /// Models from [`ADAPTIVE_SINCE`] on reject `type: "enabled"` with a 400. A
    /// version check, not an allowlist, so future releases and new families
    /// work automatically.
    fn requires_adaptive(model_id: &str) -> bool {
        claude_version(model_id).is_some_and(|(family, version)| {
            version
                >= if family == OPUS {
                    ADAPTIVE_SINCE_OPUS
                } else {
                    ADAPTIVE_SINCE
                }
        })
    }

    pub fn apply_reasoning_effort(self, body: &mut Value, dialect: &EffortDialect, model: &Model) {
        if let Some(effort) = self.effort_str(dialect, model) {
            body["reasoning_effort"] = json!(effort);
        }
    }

    pub fn apply_google_thinking(self, body: &mut Value, max: u32) {
        match self.budget(Some(max)) {
            Budgeted::Off => {}
            Budgeted::Adaptive => {
                body["generationConfig"]["thinkingConfig"] = json!({"includeThoughts": true});
            }
            Budgeted::Tokens(n) => {
                body["generationConfig"]["thinkingConfig"] = json!({"thinkingBudget": n});
            }
        }
    }

    pub fn apply_local_thinking(self, body: &mut Value, model: &Model) {
        let max = model.max_thinking_budget();
        if let Some(fields) = &model.thinking_fields
            && let Some((fragment, keep_budget)) = fields.fragment(self, max)
            && let Some(object) = body.as_object_mut()
        {
            merge_body(object, fragment);
            if keep_budget && let Budgeted::Tokens(budget) = self.budget(max) {
                body[LOCAL_BUDGET_FIELD] = json!(budget);
            }
            return;
        }
        // No fragment means the model has no way to spell this mode, so the
        // budget field takes over: a request must never end up saying nothing.
        let budget = match self.budget(max) {
            Budgeted::Off => 0,
            Budgeted::Adaptive => -1,
            Budgeted::Tokens(n) => i64::from(n),
        };
        body[LOCAL_BUDGET_FIELD] = json!(budget);
    }

    pub fn parse(input: &str, current: Self) -> Result<Self, &'static str> {
        if input.is_empty() {
            return Ok(if current.is_enabled() {
                Self::Off
            } else {
                Self::Adaptive
            });
        }
        StoredThinking::parse_setting(input)
            .map(Into::into)
            .map_err(|_| THINKING_USAGE)
    }

    pub fn status_label(self) -> Option<Cow<'static, str>> {
        match self {
            Self::Off => None,
            Self::Adaptive => Some(Cow::Borrowed("thinking")),
            Self::Effort(e) => Some(Cow::Owned(format!("thinking: {e}"))),
            Self::Budget(n) => Some(Cow::Owned(format!("thinking: {n}"))),
        }
    }
}

impl std::fmt::Display for ThinkingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => f.write_str("off"),
            Self::Adaptive => f.write_str("adaptive"),
            Self::Effort(e) => f.write_str(e.as_str()),
            Self::Budget(n) => write!(f, "{n}"),
        }
    }
}

impl From<StoredThinking> for ThinkingConfig {
    fn from(s: StoredThinking) -> Self {
        match s {
            StoredThinking::Off => Self::Off,
            StoredThinking::Adaptive => Self::Adaptive,
            StoredThinking::Effort { level } => Self::Effort(level),
            StoredThinking::Budget { tokens } => Self::Budget(tokens),
        }
    }
}

impl From<ThinkingConfig> for StoredThinking {
    fn from(c: ThinkingConfig) -> Self {
        match c {
            ThinkingConfig::Off => Self::Off,
            ThinkingConfig::Adaptive => Self::Adaptive,
            ThinkingConfig::Effort(e) => Self::Effort { level: e },
            ThinkingConfig::Budget(n) => Self::Budget { tokens: n },
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestOptions {
    pub thinking: ThinkingConfig,
    /// Raw user preference, reconciled by [`RequestOptions::clamped`] before use.
    pub fast: bool,
}

impl RequestOptions {
    /// Reconciles options with the model's capabilities. Called once before
    /// every request so UI state, restored sessions, and subagent flags all go
    /// through the same gate. Despite the name, thinking clamps both ways:
    /// down to `Off` when unsupported, up to minimal effort when required.
    pub fn clamped(self, model: &crate::model::Model) -> Self {
        Self {
            thinking: if !model.supports_thinking() {
                ThinkingConfig::Off
            } else if model.requires_thinking() && !self.thinking.is_enabled() {
                ThinkingConfig::Effort(Effort::Minimal)
            } else {
                self.thinking
            },
            fast: self.fast && model.supports_fast(),
        }
    }
}

#[derive(Debug)]
pub struct StreamResponse {
    pub message: Message,
    pub usage: TokenUsage,
    pub stop_reason: Option<StopReason>,
}

/// Provider-reported usage quota, independent of local token accounting. Not every
/// provider exposes a programmatic quota endpoint; check `Provider::fetch_usage`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUsage {
    /// Subscription/plan level when the provider reports one (e.g. "lite").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub limits: Vec<UsageLimit>,
}

/// A single quota window (e.g. a 5-hour or weekly token quota).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLimit {
    /// Human-readable label for the window, provided by the provider.
    pub label: String,
    /// Usage percentage within the window, 0-100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percentage: Option<u32>,
    /// When the window resets, as epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<u64>,
    /// Extra provider-supplied context, e.g. "$2.33 spent" for usage credits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use super::*;
    use crate::model::ThinkingSupport as Support;
    use test_case::test_case;

    #[test_case("end_turn", StopReason::EndTurn   ; "end_turn")]
    #[test_case("tool_use", StopReason::ToolUse   ; "tool_use")]
    #[test_case("max_tokens", StopReason::MaxTokens ; "max_tokens")]
    #[test_case("unknown", StopReason::EndTurn    ; "unknown_defaults_to_end_turn")]
    fn stop_reason_from_anthropic(input: &str, expected: StopReason) {
        assert_eq!(StopReason::from_anthropic(input), expected);
    }

    #[test_case("stop", StopReason::EndTurn       ; "stop_maps_to_end_turn")]
    #[test_case("tool_calls", StopReason::ToolUse ; "tool_calls_maps_to_tool_use")]
    #[test_case("length", StopReason::MaxTokens   ; "length_maps_to_max_tokens")]
    #[test_case("unknown", StopReason::EndTurn    ; "unknown_defaults_to_end_turn")]
    fn stop_reason_from_openai(input: &str, expected: StopReason) {
        assert_eq!(StopReason::from_openai(input), expected);
    }

    #[test]
    fn user_with_images_text_and_images() {
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc123"));
        let msg = Message::user_with_images("hello".into(), vec![source]);
        assert_eq!(msg.content.len(), 2);
        assert!(matches!(&msg.content[0], ContentBlock::Image { .. }));
        assert!(matches!(&msg.content[1], ContentBlock::Text { text } if text == "hello"));
    }

    #[test]
    fn user_with_images_empty_text_only_images() {
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc123"));
        let msg = Message::user_with_images(String::new(), vec![source]);
        assert_eq!(msg.content.len(), 1);
        assert!(matches!(&msg.content[0], ContentBlock::Image { .. }));
    }

    #[test]
    fn message_kind_is_backward_compatible() {
        let old: Message = serde_json::from_value(json!({
            "role": "user",
            "content": [{ "type": "text", "text": "hello" }]
        }))
        .unwrap();
        assert_eq!(old.kind, MessageKind::Turn);

        let turn = serde_json::to_value(Message::user("hello".into())).unwrap();
        assert!(turn.get("kind").is_none());

        let observation = Message::observation("built".into());
        assert_eq!(observation.first_user_text(), None);
        let observation = serde_json::to_value(observation).unwrap();
        assert_eq!(observation["kind"], "observation");
    }

    #[test_case(ImageMediaType::Png,  "image/png"  ; "png")]
    #[test_case(ImageMediaType::Jpeg, "image/jpeg" ; "jpeg")]
    #[test_case(ImageMediaType::Gif,  "image/gif"  ; "gif")]
    #[test_case(ImageMediaType::Webp, "image/webp" ; "webp")]
    fn image_source_data_url(media: ImageMediaType, mime: &str) {
        let source = ImageSource::new(media, Arc::from("dGVzdA=="));
        assert_eq!(source.to_data_url(), format!("data:{mime};base64,dGVzdA=="));
    }

    #[test_case("image/png",  Some(ImageMediaType::Png)  ; "png")]
    #[test_case("image/webp", Some(ImageMediaType::Webp) ; "webp")]
    #[test_case("image/bmp",  None                       ; "unsupported")]
    fn media_type_from_mime(mime: &str, expected: Option<ImageMediaType>) {
        assert_eq!(ImageMediaType::from_mime(mime), expected);
    }

    #[test]
    fn adapt_images_borrows_when_model_has_vision_or_no_images() {
        let model = clamp_test_model(crate::provider::ProviderKind::Anthropic);
        let with_image = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Image {
                source: ImageSource::new(ImageMediaType::Png, Arc::from("abc123")),
            }],
            ..Default::default()
        }];
        assert!(matches!(
            adapt_images_for_model(&model, &with_image),
            Cow::Borrowed(_)
        ));

        let mut text_only_model = model;
        text_only_model.supports_vision_override = Some(false);
        let no_images = vec![Message::user("hi".into())];
        assert!(matches!(
            adapt_images_for_model(&text_only_model, &no_images),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn adapt_images_replaces_blocks_for_text_only_model() {
        let mut model = clamp_test_model(crate::provider::ProviderKind::Anthropic);
        model.supports_vision_override = Some(false);
        let messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "[image: pic.png 1KB]".into(),
                    is_error: false,
                },
                ContentBlock::Image {
                    source: ImageSource::new(ImageMediaType::Png, Arc::from("abc123")),
                },
            ],
            ..Default::default()
        }];
        let adapted = adapt_images_for_model(&model, &messages);
        assert_eq!(adapted[0].content.len(), 2);
        assert!(matches!(
            &adapted[0].content[0],
            ContentBlock::ToolResult { .. }
        ));
        assert!(
            matches!(&adapted[0].content[1], ContentBlock::Text { text } if text == IMAGE_OMITTED_NOTE)
        );
    }

    #[test]
    fn image_source_serde_injects_type_base64() {
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc123"));
        let json = serde_json::to_value(&source).unwrap();
        assert_eq!(json["type"], "base64");
        assert_eq!(json["media_type"], "image/png");
        assert_eq!(json["data"], "abc123");
        let deserialized: ImageSource = serde_json::from_value(json).unwrap();
        assert_eq!(deserialized.media_type, ImageMediaType::Png);
        assert_eq!(&*deserialized.data, "abc123");
    }

    use Effort::{High, Low, Max, Minimal, XHigh};

    /// `max_output_tokens: 8192`, so `max_thinking_budget()` is 4096.
    fn thinking_model(id: &str) -> crate::model::Model {
        crate::model::Model {
            id: id.into(),
            ..clamp_test_model(crate::provider::ProviderKind::Anthropic)
        }
    }

    fn native_thinking_model(id: &str, fields: Value) -> crate::model::Model {
        let mut model = thinking_model(id);
        model.thinking_fields = Some(Box::new(serde_json::from_value(fields).unwrap()));
        model
    }

    fn native_effort_model() -> crate::model::Model {
        native_thinking_model(
            "local-model",
            json!({
                "off": {"reasoning_effort": "none"},
                "adaptive": {"reasoning_effort": "medium"},
                "low": {"reasoning_effort": "low"},
                "medium": {"reasoning_effort": "medium"},
                "xhigh": {"reasoning_effort": "xhigh"}
            }),
        )
    }

    #[test]
    fn dialects_have_non_empty_ascending_supported() {
        let all = [
            &dialect::STANDARD,
            &dialect::PREFER_HIGH,
            &dialect::HIGH_ONLY,
            &dialect::GLM,
            &dialect::DEEPSEEK,
            &dialect::ANTHROPIC_ADAPTIVE,
            &dialect::TENSORX,
            &dialect::GROK,
        ];
        for d in all {
            assert!(!d.supported.is_empty());
            for pair in d.supported.windows(2) {
                assert!(pair[0] < pair[1], "supported must be strictly ascending");
            }
            if let Some(adaptive) = d.adaptive {
                assert!(d.supported.contains(&adaptive));
            }
        }
    }

    #[test_case(ThinkingConfig::Off, "claude-opus-4-5", json!({}) ; "off")]
    #[test_case(ThinkingConfig::Adaptive, "claude-opus-4-5", json!({"thinking": {"type": "adaptive"}}) ; "adaptive")]
    #[test_case(ThinkingConfig::Budget(2048), "claude-opus-4-5", json!({"thinking": {"type": "enabled", "budget_tokens": 2048}}) ; "budget_legacy_in_range")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-opus-4-5", json!({"thinking": {"type": "enabled", "budget_tokens": 4096}}) ; "budget_legacy_clamped_to_max")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-sonnet-4-6", json!({"thinking": {"type": "enabled", "budget_tokens": 4096}}) ; "budget_legacy_sonnet")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-opus-4-6", json!({"thinking": {"type": "enabled", "budget_tokens": 4096}}) ; "budget_legacy_opus_4_6")]
    #[test_case(ThinkingConfig::Off, "claude-opus-4-7", json!({}) ; "off_adaptive_model")]
    #[test_case(ThinkingConfig::Adaptive, "claude-opus-4-7", json!({"thinking": {"type": "adaptive", "display": "summarized"}}) ; "adaptive_adaptive_model")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-opus-4-7", json!({"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "high"}}) ; "budget_adaptive_opus_4_7")]
    #[test_case(ThinkingConfig::Effort(Low), "claude-opus-4-7", json!({"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "low"}}) ; "effort_low_passthrough")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-opus-4-8-1m", json!({"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "high"}}) ; "budget_adaptive_opus_4_8_long_context")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-opus-5-1m", json!({"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "high"}}) ; "budget_adaptive_opus_5_unparsable_minor")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-opus-4.7", json!({"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "high"}}) ; "budget_adaptive_copilot_dotted_id")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-sonnet-5", json!({"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "high"}}) ; "budget_adaptive_sonnet_5")]
    #[test_case(ThinkingConfig::Budget(10000), "anthropic/claude-opus-4-7", json!({"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "high"}}) ; "budget_adaptive_gateway_prefixed_id")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-3-5-sonnet-20241022", json!({"thinking": {"type": "enabled", "budget_tokens": 4096}}) ; "budget_legacy_dated_id")]
    fn thinking_apply_to_body(config: ThinkingConfig, model_id: &str, expected: Value) {
        let mut body = json!({});
        config.apply_to_body(&mut body, &thinking_model(model_id));
        assert_eq!(body, expected);
    }

    #[test_case(&dialect::STANDARD, ThinkingConfig::Off,             None            ; "standard_off_noop")]
    #[test_case(&dialect::STANDARD, ThinkingConfig::Adaptive,        Some("medium")  ; "standard_adaptive")]
    #[test_case(&dialect::STANDARD, ThinkingConfig::Effort(Minimal), Some("minimal") ; "standard_minimal_passthrough")]
    #[test_case(&dialect::STANDARD, ThinkingConfig::Effort(Max),     Some("high")    ; "standard_max_snaps_down")]
    #[test_case(&dialect::STANDARD, ThinkingConfig::Budget(1024),    Some("medium")  ; "standard_quarter_budget")]
    #[test_case(&dialect::PREFER_HIGH, ThinkingConfig::Adaptive,        Some("high") ; "prefer_high_adaptive")]
    #[test_case(&dialect::HIGH_ONLY, ThinkingConfig::Adaptive,        Some("high") ; "high_only_adaptive")]
    #[test_case(&dialect::HIGH_ONLY, ThinkingConfig::Effort(Minimal), Some("high") ; "high_only_minimal")]
    #[test_case(&dialect::GLM, ThinkingConfig::Off,          Some("none")  ; "glm_off_explicit_none")]
    #[test_case(&dialect::GLM, ThinkingConfig::Adaptive,     Some("high")  ; "glm_adaptive")]
    #[test_case(&dialect::GLM, ThinkingConfig::Effort(Max),  Some("xhigh") ; "glm_max_snaps_to_xhigh")]
    #[test_case(&dialect::DEEPSEEK, ThinkingConfig::Adaptive,        None        ; "deepseek_adaptive_uses_api_default")]
    #[test_case(&dialect::DEEPSEEK, ThinkingConfig::Effort(Minimal), Some("max") ; "deepseek_minimal")]
    #[test_case(&dialect::ANTHROPIC_ADAPTIVE, ThinkingConfig::Adaptive,      None         ; "anthropic_adaptive_is_native")]
    #[test_case(&dialect::ANTHROPIC_ADAPTIVE, ThinkingConfig::Effort(XHigh), Some("high") ; "anthropic_xhigh_snaps_down")]
    #[test_case(&dialect::TENSORX, ThinkingConfig::Off,             Some("none") ; "tensorx_off_explicit_none")]
    fn thinking_apply_reasoning_effort(
        dialect: &EffortDialect,
        config: ThinkingConfig,
        expected: Option<&str>,
    ) {
        let mut body = json!({"model": "test"});
        config.apply_reasoning_effort(&mut body, dialect, &thinking_model("test-model"));
        match expected {
            Some(e) => assert_eq!(body["reasoning_effort"], e),
            None => assert!(body.get("reasoning_effort").is_none()),
        }
    }

    #[test_case(ThinkingConfig::Off,             Some(4096), Budgeted::Off            ; "off")]
    #[test_case(ThinkingConfig::Adaptive,        Some(4096), Budgeted::Adaptive       ; "adaptive")]
    #[test_case(ThinkingConfig::Effort(Max),     Some(4096), Budgeted::Tokens(4096)   ; "effort_delegates_to_level_budget")]
    #[test_case(ThinkingConfig::Budget(2048),    Some(4096), Budgeted::Tokens(2048)   ; "budget_in_range")]
    #[test_case(ThinkingConfig::Budget(512),     Some(4096), Budgeted::Tokens(1024)   ; "budget_floored")]
    #[test_case(ThinkingConfig::Budget(10000),   Some(4096), Budgeted::Tokens(4096)   ; "budget_clamped_to_max")]
    #[test_case(ThinkingConfig::Budget(2048),    Some(512),  Budgeted::Tokens(1024)   ; "tiny_max_raised_to_floor")]
    #[test_case(ThinkingConfig::Budget(16384),   None,       Budgeted::Tokens(16384)  ; "unknown_max_passes_budget_through")]
    #[test_case(ThinkingConfig::Budget(512),     None,       Budgeted::Tokens(1024)   ; "unknown_max_still_floors")]
    #[test_case(ThinkingConfig::Effort(Max),     None,       Budgeted::Tokens(32_768) ; "unknown_max_effort_scales_fallback")]
    #[test_case(ThinkingConfig::Effort(Minimal), None,       Budgeted::Tokens(3_276)  ; "unknown_max_minimal_effort")]
    fn thinking_budget_resolver(config: ThinkingConfig, max: Option<u32>, expected: Budgeted) {
        assert_eq!(config.budget(max), expected);
    }

    #[test_case(ThinkingConfig::Off,          json!({})                                                                  ; "off")]
    #[test_case(ThinkingConfig::Adaptive,     json!({"generationConfig": {"thinkingConfig": {"includeThoughts": true}}}) ; "adaptive")]
    #[test_case(ThinkingConfig::Budget(4096), json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": 4096}}}) ; "budget")]
    #[test_case(ThinkingConfig::Budget(10000), json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": 8192}}}) ; "budget_clamped")]
    fn thinking_apply_google_thinking(config: ThinkingConfig, expected: Value) {
        let mut body = json!({});
        config.apply_google_thinking(&mut body, 8192);
        assert_eq!(body, expected);
    }

    #[test_case(ThinkingConfig::Off,            0    ; "off")]
    #[test_case(ThinkingConfig::Adaptive,       -1   ; "adaptive")]
    #[test_case(ThinkingConfig::Budget(4096),   4096 ; "budget")]
    #[test_case(ThinkingConfig::Budget(10000),  4096 ; "budget_clamped")]
    fn thinking_apply_local_thinking(config: ThinkingConfig, expected: i64) {
        let mut body = json!({});
        config.apply_local_thinking(&mut body, &thinking_model("local-model"));
        assert_eq!(body["thinking_budget_tokens"], expected);
    }

    #[test_case(ThinkingConfig::Off,           json!({"reasoning_effort": "none"})   ; "off")]
    #[test_case(ThinkingConfig::Adaptive,      json!({"reasoning_effort": "medium"}) ; "adaptive")]
    #[test_case(ThinkingConfig::Effort(Low),   json!({"reasoning_effort": "low"})    ; "low")]
    #[test_case(ThinkingConfig::Effort(High),  json!({"reasoning_effort": "medium"}) ; "undeclared_high_snaps_down")]
    #[test_case(ThinkingConfig::Effort(XHigh), json!({"reasoning_effort": "xhigh"})  ; "xhigh")]
    #[test_case(ThinkingConfig::Budget(4096),  json!({"reasoning_effort": "xhigh"})  ; "numeric_budget_maps_to_declared_level")]
    fn local_native_effort_uses_declared_levels(config: ThinkingConfig, expected: Value) {
        let mut body = json!({});
        config.apply_local_thinking(&mut body, &native_effort_model());
        assert_eq!(body, expected);
    }

    #[test]
    fn local_required_thinking_maps_off_to_lowest_native_effort() {
        let mut model = native_effort_model();
        model.thinking_override = Some(Support::Required);
        let thinking = RequestOptions {
            thinking: ThinkingConfig::Off,
            fast: false,
        }
        .clamped(&model)
        .thinking;
        let mut body = json!({});
        thinking.apply_local_thinking(&mut body, &model);
        assert_eq!(body, json!({"reasoning_effort": "low"}));
    }

    #[test_case(ThinkingConfig::Off,          json!({"chat_template_kwargs": {"enable_thinking": false, "keep": 1}}) ; "off")]
    #[test_case(ThinkingConfig::Adaptive,     json!({"chat_template_kwargs": {"enable_thinking": true, "keep": 1}})  ; "adaptive")]
    #[test_case(ThinkingConfig::Effort(High), json!({"chat_template_kwargs": {"enable_thinking": true, "keep": 1}})  ; "effort_without_levels_uses_adaptive")]
    #[test_case(ThinkingConfig::Budget(2048), json!({"chat_template_kwargs": {"enable_thinking": true, "keep": 1}, "thinking_budget_tokens": 2048}) ; "numeric_budget")]
    fn local_native_toggle_merges_into_nested_object(config: ThinkingConfig, expected: Value) {
        let model = native_thinking_model(
            "local-toggle-model",
            json!({
                "off": {"chat_template_kwargs": {"enable_thinking": false}},
                "adaptive": {"chat_template_kwargs": {"enable_thinking": true}}
            }),
        );
        let mut body = json!({"chat_template_kwargs": {"keep": 1}});
        config.apply_local_thinking(&mut body, &model);
        assert_eq!(body, expected);
    }

    /// A mode the model has no fragment for must still reach the server, so
    /// the budget field takes over instead of the request saying nothing.
    #[test_case(json!({"low": {"reasoning_effort": "low"}}), ThinkingConfig::Off, 0 ; "off_without_off")]
    #[test_case(json!({"adaptive": {"enable_thinking": true}}), ThinkingConfig::Off, 0 ; "toggle_without_off")]
    #[test_case(json!({"off": {"reasoning_effort": "none"}}), ThinkingConfig::Adaptive, -1 ; "adaptive_without_adaptive")]
    #[test_case(json!({"off": {"reasoning_effort": "none"}}), ThinkingConfig::Budget(4096), 4096 ; "budget_without_levels")]
    fn local_native_missing_fragment_falls_back_to_budget(
        fields: Value,
        config: ThinkingConfig,
        expected: i64,
    ) {
        let model = native_thinking_model("local-partial", fields);
        let mut body = json!({});
        config.apply_local_thinking(&mut body, &model);
        assert_eq!(body, json!({ "thinking_budget_tokens": expected }));
    }

    /// llama.cpp models have no known output window; the budget the user
    /// asked for must reach the server untouched.
    #[test]
    fn local_thinking_unknown_window_passes_budget_through() {
        let mut model = thinking_model("llama-cpp-model");
        model.max_output_tokens = None;
        let mut body = json!({});
        ThinkingConfig::Budget(16_384).apply_local_thinking(&mut body, &model);
        assert_eq!(body["thinking_budget_tokens"], 16_384);
    }

    fn clamp_test_model(provider: crate::provider::ProviderKind) -> crate::model::Model {
        crate::model::Model {
            id: "test-model".into(),
            provider: std::sync::Arc::<str>::from(provider.to_string()),
            tier: crate::model::ModelTier::Medium,
            family: provider.family(),
            supports_tool_examples_override: None,
            thinking_override: None,
            supports_vision_override: Some(provider.family().supports_vision()),
            pricing: crate::model::ModelPricing::default(),
            max_output_tokens: Some(8192),
            context_window: 200_000,
            thinking_fields: None,
        }
    }

    #[test_case(None,                    ThinkingConfig::Adaptive, ThinkingConfig::Adaptive        ; "provider_default_keeps")]
    #[test_case(Some(Support::No),       ThinkingConfig::Adaptive, ThinkingConfig::Off             ; "unsupported_clamps_off")]
    #[test_case(Some(Support::Yes),      ThinkingConfig::Off,      ThinkingConfig::Off             ; "supported_keeps_off")]
    #[test_case(Some(Support::Required), ThinkingConfig::Off,      ThinkingConfig::Effort(Minimal) ; "required_raises_off_to_minimal")]
    #[test_case(Some(Support::Required), ThinkingConfig::Adaptive, ThinkingConfig::Adaptive        ; "required_keeps_enabled")]
    fn request_options_clamped_thinking(
        thinking_override: Option<Support>,
        thinking: ThinkingConfig,
        expected: ThinkingConfig,
    ) {
        let mut model = clamp_test_model(crate::provider::ProviderKind::Anthropic);
        model.thinking_override = thinking_override;
        let opts = RequestOptions {
            thinking,
            fast: false,
        };
        assert_eq!(opts.clamped(&model).thinking, expected);
    }

    #[test]
    fn request_options_clamped_fast_requires_model_support() {
        let model = clamp_test_model(crate::provider::ProviderKind::Google);
        let opts = RequestOptions {
            thinking: ThinkingConfig::Off,
            fast: true,
        };
        assert!(!opts.clamped(&model).fast);
    }

    #[test_case("",         ThinkingConfig::Off,      Ok(ThinkingConfig::Adaptive)  ; "toggle_on")]
    #[test_case("",         ThinkingConfig::Adaptive, Ok(ThinkingConfig::Off)       ; "toggle_off")]
    #[test_case("off",      ThinkingConfig::Adaptive, Ok(ThinkingConfig::Off)       ; "explicit_off")]
    #[test_case("adaptive", ThinkingConfig::Off,      Ok(ThinkingConfig::Adaptive)  ; "explicit_adaptive")]
    #[test_case("high",     ThinkingConfig::Off,      Ok(ThinkingConfig::Effort(High)) ; "explicit_effort")]
    #[test_case("8192",     ThinkingConfig::Off,      Ok(ThinkingConfig::Budget(8192)) ; "explicit_budget")]
    #[test_case("512",      ThinkingConfig::Off,      Ok(ThinkingConfig::Budget(512)) ; "small_budget")]
    #[test_case("0",        ThinkingConfig::Off,      Err(())                       ; "budget_zero")]
    #[test_case("garbage",  ThinkingConfig::Off,      Err(())                       ; "invalid_input")]
    fn thinking_parse(input: &str, current: ThinkingConfig, expected: Result<ThinkingConfig, ()>) {
        let result = ThinkingConfig::parse(input, current).map_err(|_| ());
        assert_eq!(result, expected);
    }

    #[test_case(ThinkingConfig::Off      ; "off")]
    #[test_case(ThinkingConfig::Adaptive ; "adaptive")]
    #[test_case(ThinkingConfig::Effort(Max) ; "effort")]
    #[test_case(ThinkingConfig::Budget(8192) ; "budget")]
    fn thinking_display_round_trip(config: ThinkingConfig) {
        let s = config.to_string();
        let parsed = ThinkingConfig::parse(&s, ThinkingConfig::Off).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn thinking_serde_no_signature_omits_field() {
        let block = ContentBlock::Thinking {
            thinking: "x".into(),
            signature: None,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert!(json.get("signature").is_none());
    }
}
