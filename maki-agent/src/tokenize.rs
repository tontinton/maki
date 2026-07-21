use serde_json::Value;
use tiktoken_rs::cl100k_base_singleton;

const TIKTOKEN_MAX_CHARS: usize = 4_096;
const BYTES_PER_TOKEN_ESTIMATE: usize = 4;

#[must_use]
pub fn count_tokens(text: &str) -> usize {
    if text.len() > TIKTOKEN_MAX_CHARS {
        return text.len() / BYTES_PER_TOKEN_ESTIMATE;
    }
    cl100k_base_singleton().encode_ordinary(text).len()
}

#[must_use]
pub fn count_json(value: &Value) -> usize {
    match serde_json::to_string(value) {
        Ok(text) => count_tokens(&text),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn counts_short_text() {
        let text = "hello world";
        let tokens = count_tokens(text);
        assert!(
            tokens > 0 && tokens < 10,
            "expected positive token count, got {tokens}"
        );
    }

    #[test]
    fn counts_json_object() {
        let value = json!({"name": "skill", "list": true});
        let tokens = count_json(&value);
        assert!(tokens > 0, "expected positive token count for json");
    }

    #[test]
    fn counts_long_text_uses_byte_fallback() {
        let text = "x".repeat(10_000);
        let tokens = count_tokens(&text);
        assert_eq!(
            tokens, 2_500,
            "long repeated text should use bytes/4 fallback"
        );
    }

    #[test]
    fn counts_large_json_uses_byte_fallback() {
        let value = json!({"data": "x".repeat(10_000)});
        let tokens = count_json(&value);
        assert!(
            tokens > 2_000,
            "large json should produce positive token count"
        );
    }

    #[test]
    fn counts_empty_text() {
        let tokens = count_tokens("");
        assert_eq!(tokens, 0, "empty string should have zero tokens");
    }

    #[test]
    fn counts_non_ascii_text() {
        let text = "héllo wörld 世界";
        let tokens = count_tokens(text);
        assert!(tokens > 0, "non-ascii text should have positive tokens");
    }

    #[test]
    fn uses_tiktoken_up_to_threshold_and_fallback_after() {
        let at_threshold = "x".repeat(TIKTOKEN_MAX_CHARS);
        let over_threshold = "x".repeat(TIKTOKEN_MAX_CHARS + 1);

        let at_tokens = count_tokens(&at_threshold);
        let over_tokens = count_tokens(&over_threshold);

        assert!(at_tokens > 0, "text at threshold should be counted");
        assert_eq!(
            over_tokens,
            over_threshold.len() / BYTES_PER_TOKEN_ESTIMATE,
            "text over threshold should use bytes/4 fallback"
        );
    }

    #[test]
    fn count_json_falls_back_for_non_serializable() {
        let value = Value::Null;
        let tokens = count_json(&value);
        assert_eq!(tokens, 1, "null serializes to one token-ish string");
    }
}
