use std::time::Duration;

use isahc::config::Configurable;
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use maki_tool_macro::Tool;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::ToolOutput;

use super::truncate_output;
use tracing::info;

const EXA_MCP_ENDPOINT: &str = "https://mcp.exa.ai/mcp";
const REQUEST_TIMEOUT_SECS: u64 = 25;
const DEFAULT_NUM_RESULTS: u64 = 8;
const NO_RESULTS_MSG: &str = "No search results found";

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct WebSearch {
    #[param(description = "Search query")]
    query: String,
    #[param(description = "Number of results to return (default 8)")]
    num_results: Option<u64>,
}

impl WebSearch {
    pub const NAME: &str = "websearch";
    pub const DESCRIPTION: &str = include_str!("websearch.md");
    pub const EXAMPLES: Option<&str> = Some(r#"[{"query": "rust async runtime comparison 2025"}]"#);

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        ctx.cancel
            .race(self.do_search(ctx.deadline, &ctx.config))
            .await?
    }

    async fn do_search(
        &self,
        deadline: super::Deadline,
        config: &maki_config::AgentConfig,
    ) -> Result<ToolOutput, String> {
        let num_results = self.num_results.unwrap_or(DEFAULT_NUM_RESULTS);

        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "web_search_exa",
                "arguments": {
                    "query": self.query,
                    "numResults": num_results,
                    "type": "auto",
                    "livecrawl": "fallback",
                }
            }
        });

        let timeout_secs = deadline.cap_timeout(REQUEST_TIMEOUT_SECS)?;
        let client = HttpClient::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| format!("client error: {e}"))?;

        let json_body = serde_json::to_vec(&payload).map_err(|e| format!("json error: {e}"))?;
        let request = Request::builder()
            .method("POST")
            .uri(EXA_MCP_ENDPOINT)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(json_body)
            .map_err(|e| format!("request build error: {e}"))?;

        let mut response = client
            .send_async(request)
            .await
            .map_err(|e| format!("request failed: {e}"))?;

        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| format!("read error: {e}"))?;

        info!(query = %self.query, num_results, status, body_bytes = body.len(), "websearch response");

        if body.len() > config.max_response_bytes {
            return Err(format!("response too large: {} bytes", body.len()));
        }

        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}: {}", &body[..body.len().min(200)]));
        }

        let text = parse_sse_response(&body)?;
        Ok(ToolOutput::Plain(truncate_output(
            text,
            config.max_output_lines,
            config.max_output_bytes,
        )))
    }

    pub fn start_summary(&self) -> String {
        self.query.clone()
    }
}

super::impl_tool!(
    WebSearch,
    audience = super::ToolAudience::MAIN | super::ToolAudience::INTERPRETER,
);

impl super::ToolInvocation for WebSearch {
    fn start_summary(&self) -> super::SummaryFuture {
        super::SummaryFuture::Ready(WebSearch::start_summary(self))
    }
    fn permission_scope(&self) -> Option<String> {
        Some(self.query.clone())
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { WebSearch::execute(&self, ctx).await })
    }
}

fn parse_sse_response(body: &str) -> Result<String, String> {
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let parsed: Value =
            serde_json::from_str(data).map_err(|e| format!("SSE JSON parse error: {e}"))?;

        if let Some(text) = parsed
            .pointer("/result/content")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(|item| item["text"].as_str())
            && !text.is_empty()
        {
            return Ok(text.to_string());
        }
    }
    Ok(NO_RESULTS_MSG.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn sse_line(result_json: &Value) -> String {
        format!("data: {result_json}")
    }

    fn make_sse(text: &str) -> String {
        sse_line(&json!({
            "jsonrpc": "2.0",
            "result": {
                "content": [{"type": "text", "text": text}]
            }
        }))
    }

    #[test]
    fn parse_sse_response_extracts_text() {
        let body = format!(
            "event: message\n{}\n",
            make_sse("Rust is a systems language")
        );
        let result = parse_sse_response(&body).unwrap();
        assert_eq!(result, "Rust is a systems language");
    }

    #[test]
    fn parse_sse_response_first_data_line_wins() {
        let body = format!("{}\n{}\n", make_sse("first"), make_sse("second"));
        assert_eq!(parse_sse_response(&body).unwrap(), "first");
    }

    #[test_case(""                              ; "empty_body")]
    #[test_case(&sse_line(&json!({"result": {"content": []}})) ; "empty_content_array")]
    #[test_case(&sse_line(&json!({"result": {}})) ; "missing_content_key")]
    fn parse_sse_response_no_results(input: &str) {
        assert_eq!(parse_sse_response(input).unwrap(), NO_RESULTS_MSG);
    }

    #[test]
    fn parse_sse_response_empty_text_falls_through() {
        let body = format!("{}\n{}", make_sse(""), make_sse("actual result"));
        assert_eq!(parse_sse_response(&body).unwrap(), "actual result");
    }

    #[test]
    fn parse_sse_response_malformed_json_is_error() {
        let body = "data: {not valid json}";
        assert!(parse_sse_response(body).is_err());
    }
}
