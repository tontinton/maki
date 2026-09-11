//! Prompt size accounting.
//!
//! An estimate maki computes from message bytes, and the count the provider
//! returns with a response. [`ContextGauge`] carries one session's running
//! total, so a measurement is never re-derived from an estimate.

use serde_json::Value;

use crate::types::{ContentBlock, Message};

const CHARS_PER_TOKEN: usize = 4;
/// Flat per image, because all we have is the encoded blob, and its size
/// tracks compression rather than the tile count the provider bills. A
/// screenshot at the sizes [`crate::adapt_images_for_model`] allows lands near
/// this. Counting nothing understated exactly the transcripts most likely to
/// overflow.
const TOKENS_PER_IMAGE: u32 = 1_500;

/// Counts message content only. The system prompt and the tool schemas, a five
/// figure baseline on a full tool set, are not in here, so this must never
/// replace a context size the provider measured.
pub fn estimate_message_tokens(messages: &[Message]) -> u32 {
    if messages.is_empty() {
        return 0;
    }
    let (total_bytes, images) =
        messages
            .iter()
            .flat_map(|m| &m.content)
            .fold((0usize, 0u32), |(bytes, images), block| match block {
                ContentBlock::Text { text } => (bytes + text.len(), images),
                ContentBlock::ToolResult { content, .. } => (bytes + content.len(), images),
                ContentBlock::ToolUse { input, .. } => (bytes + json_len(input), images),
                ContentBlock::Thinking { thinking, .. } => (bytes + thinking.len(), images),
                ContentBlock::Image { .. } => (bytes, images + 1),
                ContentBlock::RedactedThinking { .. } => (bytes, images),
            });
    (total_bytes.max(CHARS_PER_TOKEN) / CHARS_PER_TOKEN) as u32 + images * TOKENS_PER_IMAGE
}

/// [`estimate_message_tokens`] plus the system prompt and the serialized tool
/// schemas, which a server checking `prompt + max_tokens <= context_window`
/// counts too.
pub fn estimate_prompt_tokens(messages: &[Message], system: &str, tools: &Value) -> u32 {
    let overhead = (system.len() + json_len(tools)) / CHARS_PER_TOKEN;
    estimate_message_tokens(messages).saturating_add(overhead as u32)
}

/// Serialized length without building the string, since this runs over the
/// whole transcript and the tool catalog before every request.
fn json_len(value: &Value) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    match serde_json::to_writer(&mut counter, value) {
        Ok(()) => counter.0,
        Err(_) => 0,
    }
}

/// The running prompt size of one session: the provider's own count for the
/// last prompt it was sent, plus a byte estimate of everything appended since.
///
/// The two are held apart because only one of them can be wrong, and every
/// response shrinks the guessed part back to nothing.
///
/// Lives as long as the transcript, which is longer than one agent run. A gauge
/// rebuilt per user turn throws away every measurement the session ever made
/// and is back to guessing.
#[derive(Debug, Clone, Default)]
pub struct ContextGauge {
    measured: u32,
    /// Byte estimate of the messages appended since that count.
    appended: u32,
}

impl ContextGauge {
    /// A session read back from disk was measured before it was stored, so it
    /// starts from that count rather than from an estimate of its transcript.
    pub fn restored(measured: u32) -> Self {
        Self {
            measured,
            appended: 0,
        }
    }

    pub fn size(&self) -> u32 {
        self.measured.saturating_add(self.appended)
    }

    /// Ground truth for a prompt that was just sent, from a response or from a
    /// rejection that quoted its own count. It covers every message that
    /// request carried, so it replaces the total instead of adding to it.
    ///
    /// A zero measurement means the provider reported no usage (Z.AI streams
    /// without it), not an empty prompt, so it overwrites nothing.
    pub fn record(&mut self, measured: u32) {
        if measured == 0 {
            return;
        }
        self.measured = measured;
        self.appended = 0;
    }

    /// Messages appended since the last measurement, estimated until the next
    /// response measures them along with the rest.
    pub fn append(&mut self, messages: &[Message]) {
        self.appended = self
            .appended
            .saturating_add(estimate_message_tokens(messages));
    }

    /// Starts over from a transcript nobody has sent yet, as compaction leaves
    /// behind. Whatever was measured describes messages that no longer exist.
    pub fn reset(&mut self, messages: &[Message]) {
        *self = Self::default();
        self.append(messages);
    }

    /// A resumed session can already fill the window, and the gauge only learns
    /// the real size from a response, so without this its first request goes
    /// out unguarded.
    pub fn seed_if_empty(&mut self, messages: &[Message], system: &str, tools: &Value) {
        if self.size() == 0 {
            self.appended = estimate_prompt_tokens(messages, system, tools);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::types::{ImageMediaType, ImageSource, Role};

    const MEASURED: u32 = 10_000;
    const TEXT_BYTES: usize = 4_000;
    const TEXT_TOKENS: u32 = (TEXT_BYTES / CHARS_PER_TOKEN) as u32;

    fn text_message(len: usize) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "x".repeat(len),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_measurement_replaces_the_estimate_and_later_appends_stack_on_it() {
        let mut gauge = ContextGauge::default();
        gauge.append(&[text_message(TEXT_BYTES)]);
        assert_eq!(gauge.size(), TEXT_TOKENS);

        gauge.record(MEASURED);
        assert_eq!(
            gauge.size(),
            MEASURED,
            "the estimate the measurement covers is not counted twice"
        );

        gauge.record(0);
        assert_eq!(
            gauge.size(),
            MEASURED,
            "a provider that reports no usage overwrites nothing"
        );

        gauge.append(&[text_message(TEXT_BYTES)]);
        assert_eq!(gauge.size(), MEASURED + TEXT_TOKENS);
    }

    /// The transcript compaction summarized away is gone, so the measurement
    /// describing it cannot keep sizing the session.
    #[test]
    fn a_reset_drops_the_measurement_with_the_transcript() {
        let mut gauge = ContextGauge::restored(MEASURED);
        gauge.reset(&[text_message(TEXT_BYTES)]);
        assert_eq!(gauge.size(), TEXT_TOKENS);
    }

    #[test]
    fn seeding_only_fills_an_empty_gauge() {
        let messages = [text_message(TEXT_BYTES)];
        let mut gauge = ContextGauge::default();
        gauge.seed_if_empty(&messages, "", &json!([]));
        assert_eq!(gauge.size(), TEXT_TOKENS);

        gauge.seed_if_empty(&[text_message(TEXT_BYTES * 10)], "", &json!([]));
        assert_eq!(gauge.size(), TEXT_TOKENS, "a seeded gauge is not reseeded");
    }

    #[test]
    fn the_prompt_estimate_covers_the_system_prompt_and_the_tool_schemas() {
        let messages = [text_message(TEXT_BYTES)];
        let system = "s".repeat(TEXT_BYTES);
        assert!(
            estimate_prompt_tokens(&messages, &system, &json!([{"name": "read"}]))
                > estimate_message_tokens(&messages) + TEXT_TOKENS
        );
    }

    #[test]
    fn images_are_charged_even_though_their_bytes_are_not() {
        let with_image = [Message {
            role: Role::User,
            content: vec![ContentBlock::Image {
                source: ImageSource::new(ImageMediaType::Png, "".into()),
            }],
            ..Default::default()
        }];
        let empty_text = [text_message(0)];
        assert_eq!(
            estimate_message_tokens(&with_image) - estimate_message_tokens(&empty_text),
            TOKENS_PER_IMAGE
        );
    }
}
