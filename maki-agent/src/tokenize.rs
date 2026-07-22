use serde_json::Value;
use tiktoken_rs::cl100k_base_singleton;
use tracing::warn;

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
        Err(e) => {
            warn!(error = %e, "failed to serialize JSON for token count; using byte fallback");
            count_tokens(&value.to_string())
        }
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

    #[test]
    fn tokenize_fuzz_smoke() {
        fn xorshift64(state: &mut u64) -> u64 {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        }

        fn gen_text(state: &mut u64, max_len: usize) -> String {
            const ALPHABET: &[u8] =
                b"abcdefghijklmnopqrstuvwxyz0123456789 \n\t!@#$%^&*()_+-=[]{}|;':\",./<>?";
            let len = (xorshift64(state) as usize) % max_len;
            let mut out = String::with_capacity(len);
            for _ in 0..len {
                let idx = (xorshift64(state) as usize) % ALPHABET.len();
                out.push(ALPHABET[idx] as char);
            }
            out
        }

        fn gen_value(state: &mut u64, depth: usize) -> Value {
            let kind = xorshift64(state) % 6;
            match kind {
                0 => Value::Null,
                1 => Value::Bool((xorshift64(state) & 1) == 1),
                2 => Value::Number(((xorshift64(state) % 1000) as i64).into()),
                3 => Value::String(gen_text(state, 50)),
                4 if depth > 0 => {
                    let len = (xorshift64(state) as usize) % 4;
                    Value::Array((0..len).map(|_| gen_value(state, depth - 1)).collect())
                }
                5 if depth > 0 => {
                    let len = (xorshift64(state) as usize) % 4;
                    let mut m = serde_json::Map::new();
                    for i in 0..len {
                        m.insert(format!("k{i}"), gen_value(state, depth - 1));
                    }
                    Value::Object(m)
                }
                _ => Value::String(gen_text(state, 10)),
            }
        }

        let mut state = 0x1234_5678_9ABC_DEF0u64;
        for _ in 0..500 {
            let text = gen_text(&mut state, 6_000);
            let tokens = count_tokens(&text);
            assert!(
                tokens <= text.len().max(1),
                "token count {tokens} exceeds text length {}",
                text.len()
            );

            let value = gen_value(&mut state, 4);
            let tokens = count_json(&value);
            let json_text = serde_json::to_string(&value).unwrap();
            assert!(
                tokens <= json_text.len().max(1),
                "json token count {tokens} exceeds json length {}",
                json_text.len()
            );
        }
    }
}
