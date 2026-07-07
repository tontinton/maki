//! Message and content types for provider communication.
//! `Message.display_text`: `Some("")` marks a message as synthetic (sent to the API but hidden
//! from the UI). `user_text()` returns `None` for these, so system-injected messages
//! (cancel markers, compaction prompts) stay invisible without a separate type.

use std::borrow::Cow;
use std::sync::Arc;

use maki_storage::sessions::{StoredThinking, TitleSource};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use strum::{Display, IntoStaticStr};
use tracing::warn;

use crate::TokenUsage;
use crate::model::Model;

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

/// For models without vision, image blocks become a text note instead of a
/// wire block the API would reject. History keeps the pixels, so switching
/// back to a vision-capable model restores them.
pub fn adapt_images_for_model<'a>(model: &Model, messages: &'a [Message]) -> Cow<'a, [Message]> {
    let has_image = |m: &Message| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Image { .. }))
    };
    if model.vision || !messages.iter().any(has_image) {
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

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_text: Option<String>,
}

impl Message {
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
            ContentBlock::Text { text } if !text.is_empty() => Some(text.as_str()),
            _ => None,
        })
    }

    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
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
        if !self.role.is_user() {
            return None;
        }
        self.user_text()
    }
}

#[derive(Debug, Clone, Serialize)]
pub enum ProviderEvent {
    TextDelta { text: String },
    ThinkingDelta { text: String },
    ToolUseStart { id: String, name: String },
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

const THINKING_USAGE: &str = "Usage: /thinking [off|adaptive|<budget\u{2265}1024>]";
const GLM_XHIGH_THRESHOLD: u32 = 8192;

/// Each provider speaks a different dialect of `reasoning_effort`.
/// New providers get a variant here instead of a new body-mutating method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortScale {
    /// low / medium / high. Adaptive = medium.
    Standard,
    /// low / medium / high. Adaptive = high.
    PreferHigh,
    /// Always "high", ignores budget.
    HighOnly,
    /// none / high / xhigh. GLM reasons by default, so Off sends "none"
    /// explicitly. Only use behind `Model::supports_thinking`.
    Glm,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThinkingConfig {
    #[default]
    Off,
    Adaptive,
    Budget(u32),
}

impl ThinkingConfig {
    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    pub fn budget_to_effort(n: u32) -> &'static str {
        if n < 2048 {
            "low"
        } else if n < 8192 {
            "medium"
        } else {
            "high"
        }
    }

    pub fn apply_to_body(self, body: &mut Value, model_id: &str) {
        match self {
            Self::Off => {}
            Self::Adaptive => {
                body["thinking"] = json!({"type": "adaptive"});
            }
            Self::Budget(n) if Self::requires_adaptive(model_id) => {
                body["thinking"] = json!({"type": "adaptive"});
                body["output_config"]["effort"] = json!(Self::budget_to_effort(n));
            }
            Self::Budget(n) => {
                body["thinking"] = json!({"type": "enabled", "budget_tokens": n});
            }
        }
    }

    /// Version check, not an allowlist, so future Opus releases work automatically.
    fn requires_adaptive(model_id: &str) -> bool {
        let Some(version) = model_id.strip_prefix("claude-opus-") else {
            return false;
        };
        let mut parts = version.split('-');
        let (Some(Ok(major)), Some(Ok(minor))) = (
            parts.next().map(str::parse::<u32>),
            parts.next().map(str::parse::<u32>),
        ) else {
            return false;
        };
        (major, minor) >= (4, 7)
    }

    pub fn apply_reasoning_effort(self, body: &mut Value, scale: EffortScale) {
        let effort = match (self, scale) {
            (Self::Off, EffortScale::Glm) => "none",
            (Self::Off, _) => return,
            (Self::Adaptive, EffortScale::Standard) => "medium",
            (Self::Adaptive, _) => "high",
            (Self::Budget(_), EffortScale::HighOnly) => "high",
            (Self::Budget(n), EffortScale::Glm) => {
                if n >= GLM_XHIGH_THRESHOLD {
                    "xhigh"
                } else {
                    "high"
                }
            }
            (Self::Budget(n), EffortScale::Standard | EffortScale::PreferHigh) => {
                Self::budget_to_effort(n)
            }
        };
        body["reasoning_effort"] = json!(effort);
    }

    pub fn apply_google_thinking(self, body: &mut Value) {
        match self {
            Self::Off => {}
            Self::Adaptive => {
                body["generationConfig"]["thinkingConfig"] = json!({"includeThoughts": true});
            }
            Self::Budget(n) => {
                body["generationConfig"]["thinkingConfig"] = json!({"thinkingBudget": n});
            }
        }
    }

    pub fn apply_local_thinking(self, body: &mut Value) {
        let budget = match self {
            Self::Off => 0,
            Self::Adaptive => -1,
            Self::Budget(n) => n as i64,
        };
        body["thinking_budget_tokens"] = json!(budget);
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
            Self::Budget(n) => Some(Cow::Owned(format!("thinking: {n}"))),
        }
    }
}

impl std::fmt::Display for ThinkingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => f.write_str("off"),
            Self::Adaptive => f.write_str("adaptive"),
            Self::Budget(n) => write!(f, "{n}"),
        }
    }
}

impl From<StoredThinking> for ThinkingConfig {
    fn from(s: StoredThinking) -> Self {
        match s {
            StoredThinking::Off => Self::Off,
            StoredThinking::Adaptive => Self::Adaptive,
            StoredThinking::Budget { tokens } => Self::Budget(tokens),
        }
    }
}

impl From<ThinkingConfig> for StoredThinking {
    fn from(c: ThinkingConfig) -> Self {
        match c {
            ThinkingConfig::Off => Self::Off,
            ThinkingConfig::Adaptive => Self::Adaptive,
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
    /// Strips options the model does not support. Called once before every
    /// request so UI state, restored sessions, and subagent flags all go
    /// through the same gate.
    pub fn clamped(self, model: &crate::model::Model) -> Self {
        Self {
            thinking: if model.supports_thinking() {
                self.thinking
            } else {
                ThinkingConfig::Off
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
    pub percentage: u32,
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
        text_only_model.vision = false;
        let no_images = vec![Message::user("hi".into())];
        assert!(matches!(
            adapt_images_for_model(&text_only_model, &no_images),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn adapt_images_replaces_blocks_for_text_only_model() {
        let mut model = clamp_test_model(crate::provider::ProviderKind::Anthropic);
        model.vision = false;
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

    #[test_case(ThinkingConfig::Off, "claude-opus-4-5", json!({}) ; "off")]
    #[test_case(ThinkingConfig::Adaptive, "claude-opus-4-5", json!({"thinking": {"type": "adaptive"}}) ; "adaptive")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-opus-4-5", json!({"thinking": {"type": "enabled", "budget_tokens": 10000}}) ; "budget_legacy_old_opus")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-sonnet-4-6", json!({"thinking": {"type": "enabled", "budget_tokens": 10000}}) ; "budget_legacy_sonnet")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-opus-4-6", json!({"thinking": {"type": "enabled", "budget_tokens": 10000}}) ; "budget_legacy_opus_4_6")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-opus-4-7", json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "high"}}) ; "budget_adaptive_opus_4_7")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-opus-4-8-1m", json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "high"}}) ; "budget_adaptive_opus_4_8_long_context")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-opus-5-0", json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "high"}}) ; "budget_adaptive_future_opus_5")]
    fn thinking_apply_to_body(config: ThinkingConfig, model_id: &str, expected: Value) {
        let mut body = json!({});
        config.apply_to_body(&mut body, model_id);
        assert_eq!(body, expected);
    }

    use EffortScale::{Glm, HighOnly, PreferHigh, Standard};

    #[test_case(Standard,   ThinkingConfig::Off,          None            ; "off_noop")]
    #[test_case(Standard,   ThinkingConfig::Adaptive,     Some("medium")  ; "standard_adaptive")]
    #[test_case(Standard,   ThinkingConfig::Budget(1024), Some("low")     ; "standard_budget_low")]
    #[test_case(Standard,   ThinkingConfig::Budget(4096), Some("medium")  ; "standard_budget_medium")]
    #[test_case(Standard,   ThinkingConfig::Budget(8192), Some("high")    ; "standard_budget_high")]
    #[test_case(PreferHigh, ThinkingConfig::Adaptive,     Some("high")    ; "prefer_high_adaptive")]
    #[test_case(PreferHigh, ThinkingConfig::Budget(1024), Some("low")     ; "prefer_high_budget_low")]
    #[test_case(HighOnly,   ThinkingConfig::Adaptive,     Some("high")    ; "high_only_adaptive")]
    #[test_case(HighOnly,   ThinkingConfig::Budget(1024), Some("high")    ; "high_only_budget")]
    #[test_case(Glm,        ThinkingConfig::Off,          Some("none")    ; "glm_off_explicit_none")]
    #[test_case(Glm,        ThinkingConfig::Adaptive,     Some("high")    ; "glm_adaptive")]
    #[test_case(Glm,        ThinkingConfig::Budget(1024), Some("high")    ; "glm_budget_low")]
    #[test_case(Glm,        ThinkingConfig::Budget(8192), Some("xhigh")   ; "glm_budget_xhigh")]
    fn thinking_apply_reasoning_effort(
        scale: EffortScale,
        config: ThinkingConfig,
        expected: Option<&str>,
    ) {
        let mut body = json!({"model": "test"});
        config.apply_reasoning_effort(&mut body, scale);
        match expected {
            Some(e) => assert_eq!(body["reasoning_effort"], e),
            None => assert!(body.get("reasoning_effort").is_none()),
        }
    }

    #[test_case(ThinkingConfig::Off,          json!({})                                                                  ; "off")]
    #[test_case(ThinkingConfig::Adaptive,     json!({"generationConfig": {"thinkingConfig": {"includeThoughts": true}}}) ; "adaptive")]
    #[test_case(ThinkingConfig::Budget(4096), json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": 4096}}}) ; "budget")]
    fn thinking_apply_google_thinking(config: ThinkingConfig, expected: Value) {
        let mut body = json!({});
        config.apply_google_thinking(&mut body);
        assert_eq!(body, expected);
    }

    #[test_case(ThinkingConfig::Off,          0    ; "off")]
    #[test_case(ThinkingConfig::Adaptive,     -1   ; "adaptive")]
    #[test_case(ThinkingConfig::Budget(4096), 4096 ; "budget")]
    fn thinking_apply_local_thinking(config: ThinkingConfig, expected: i64) {
        let mut body = json!({});
        config.apply_local_thinking(&mut body);
        assert_eq!(body["thinking_budget_tokens"], expected);
    }

    fn clamp_test_model(provider: crate::provider::ProviderKind) -> crate::model::Model {
        crate::model::Model {
            id: "test-model".into(),
            provider,
            dynamic_slug: None,
            tier: crate::model::ModelTier::Medium,
            family: provider.family(),
            supports_tool_examples_override: None,
            supports_thinking_override: None,
            vision: provider.family().supports_vision(),
            pricing: crate::model::ModelPricing::default(),
            max_output_tokens: 8192,
            context_window: 200_000,
        }
    }

    #[test_case(None,        ThinkingConfig::Adaptive, ThinkingConfig::Adaptive ; "provider_default_keeps")]
    #[test_case(Some(false), ThinkingConfig::Adaptive, ThinkingConfig::Off      ; "override_false_clamps")]
    fn request_options_clamped_thinking(
        supports: Option<bool>,
        thinking: ThinkingConfig,
        expected: ThinkingConfig,
    ) {
        let mut model = clamp_test_model(crate::provider::ProviderKind::Anthropic);
        model.supports_thinking_override = supports;
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
