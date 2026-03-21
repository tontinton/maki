use std::time::Duration;

use isahc::config::Configurable;
use isahc::{AsyncReadResponseExt, HttpClient, Request};

use maki_tool_macro::Tool;

use crate::ToolOutput;

use super::truncate_output;
use tracing::info;

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 120;
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const CF_MITIGATED: &str = "cf-mitigated";
const CF_CHALLENGE: &str = "challenge";
const FALLBACK_USER_AGENT: &str = "maki";

#[derive(Tool, Debug, Clone)]
pub struct WebFetch {
    #[param(description = "URL to fetch (http:// or https://)")]
    url: String,
    #[param(description = "Output format: markdown (default), text, or html")]
    format: Option<String>,
    #[param(description = "Timeout in seconds (default 30, max 120)")]
    timeout: Option<u64>,
}

impl WebFetch {
    pub const NAME: &str = "webfetch";
    pub const DESCRIPTION: &str = include_str!("webfetch.md");
    pub const EXAMPLES: Option<&str> = None;

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        ctx.cancel
            .race(self.do_fetch(ctx.deadline, ctx.config))
            .await?
    }

    async fn do_fetch(
        &self,
        deadline: super::Deadline,
        config: maki_config::AgentConfig,
    ) -> Result<ToolOutput, String> {
        let url = validate_and_upgrade_url(&self.url)?;
        let format = self.validated_format()?;
        let base_timeout = self
            .timeout
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);
        let timeout = Duration::from_secs(deadline.cap_timeout(base_timeout)?);

        let client = HttpClient::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| format!("client error: {e}"))?;

        let accept = accept_header(format);

        let request = Request::builder()
            .method("GET")
            .uri(&url)
            .header("User-Agent", USER_AGENT)
            .header("Accept", accept)
            .body(())
            .map_err(|e| format!("request build error: {e}"))?;
        let response = client
            .send_async(request)
            .await
            .map_err(|e| format!("request failed: {e}"))?;

        let is_cf_challenge = response.status().as_u16() == 403
            && response
                .headers()
                .get(CF_MITIGATED)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains(CF_CHALLENGE));
        let mut response = if is_cf_challenge {
            info!(url = %url, "retrying with fallback user-agent after CF challenge");
            let retry_request = Request::builder()
                .method("GET")
                .uri(&url)
                .header("User-Agent", FALLBACK_USER_AGENT)
                .header("Accept", accept)
                .body(())
                .map_err(|e| format!("request build error: {e}"))?;
            client
                .send_async(retry_request)
                .await
                .map_err(|e| format!("request failed: {e}"))?
        } else {
            response
        };

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}"));
        }

        if let Some(len) = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
            && len > config.max_response_bytes
        {
            return Err(format!("response too large: {len} bytes"));
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let bytes = response
            .bytes()
            .await
            .map_err(|e| format!("read error: {e}"))?;

        info!(url = %url, status, body_bytes = bytes.len(), "webfetch response");

        if bytes.len() > config.max_response_bytes {
            return Err(format!("response too large: {} bytes", bytes.len()));
        }

        if is_image_content(&content_type) {
            return Err("image content cannot be displayed as text".into());
        }

        let text = String::from_utf8_lossy(&bytes).into_owned();
        let is_html = content_type.contains("text/html");

        let output = match format {
            "markdown" if is_html => {
                htmd::convert(&text).map_err(|e| format!("html conversion: {e}"))?
            }
            "text" if is_html => strip_html_tags(&text),
            _ => text,
        };

        Ok(ToolOutput::Plain(truncate_output(
            output,
            config.max_output_lines,
            config.max_output_bytes,
        )))
    }

    pub fn start_summary(&self) -> String {
        match self.format.as_deref() {
            Some(f) if f != "markdown" => format!("{} [{f}]", self.url),
            _ => self.url.clone(),
        }
    }
}

impl super::ToolDefaults for WebFetch {
    fn permission(&self) -> Option<String> {
        Some(self.url.clone())
    }
}

impl WebFetch {
    fn validated_format(&self) -> Result<&'static str, String> {
        match self.format.as_deref() {
            None | Some("markdown") => Ok("markdown"),
            Some("text") => Ok("text"),
            Some("html") => Ok("html"),
            Some(other) => Err(format!("unknown format: {other}")),
        }
    }
}

fn validate_and_upgrade_url(url: &str) -> Result<String, String> {
    if let Some(rest) = url.strip_prefix("http://") {
        return Ok(format!("https://{rest}"));
    }
    if url.starts_with("https://") {
        return Ok(url.to_string());
    }
    Err(format!(
        "URL must start with http:// or https://, got: {url}"
    ))
}

fn accept_header(format: &str) -> &'static str {
    match format {
        "html" => "text/html,*/*;q=0.5",
        "text" => "text/plain,text/html;q=0.9,*/*;q=0.5",
        _ => "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.5",
    }
}

fn is_image_content(content_type: &str) -> bool {
    content_type.starts_with("image/") && !content_type.contains("svg")
}

fn strip_html_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut skip_tag: Option<&'static str> = None;
    let mut tag_buf = String::new();

    for ch in html.chars() {
        if ch == '<' {
            in_tag = true;
            tag_buf.clear();
            continue;
        }
        if ch == '>' {
            in_tag = false;
            let tag_lower = tag_buf.to_ascii_lowercase();
            let tag_name = tag_lower.split_whitespace().next().unwrap_or("");

            match skip_tag {
                None => {
                    skip_tag = match tag_name {
                        "script" => Some("script"),
                        "style" => Some("style"),
                        "noscript" => Some("noscript"),
                        _ => None,
                    };
                }
                Some(expected) => {
                    if tag_name.strip_prefix('/') == Some(expected) {
                        skip_tag = None;
                    }
                }
            }

            if skip_tag.is_none() && !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
            continue;
        }
        if in_tag {
            tag_buf.push(ch);
            continue;
        }
        if skip_tag.is_some() {
            continue;
        }
        if ch.is_whitespace() {
            if !out.ends_with(' ') && !out.is_empty() {
                out.push(' ');
            }
        } else {
            out.push(ch);
        }
    }

    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("https://example.com", "https://example.com" ; "https_passthrough")]
    #[test_case("http://example.com", "https://example.com" ; "http_upgraded_to_https")]
    fn validate_and_upgrade_url_valid(input: &str, expected: &str) {
        assert_eq!(validate_and_upgrade_url(input).unwrap(), expected);
    }

    #[test_case("ftp://example.com" ; "unsupported_scheme")]
    #[test_case("example.com" ; "bare_domain")]
    fn validate_and_upgrade_url_invalid(input: &str) {
        assert!(validate_and_upgrade_url(input).is_err());
    }

    #[test_case(None,          Ok("markdown") ; "defaults_to_markdown")]
    #[test_case(Some("text"),    Ok("text")     ; "valid_text")]
    #[test_case(Some("html"),    Ok("html")     ; "valid_html")]
    #[test_case(Some("xml"),     Err(())        ; "rejects_unknown")]
    fn validated_format_cases(format: Option<&str>, expected: Result<&str, ()>) {
        let wf = WebFetch {
            url: "https://x.com".into(),
            format: format.map(Into::into),
            timeout: None,
        };
        match expected {
            Ok(fmt) => assert_eq!(wf.validated_format().unwrap(), fmt),
            Err(()) => assert!(wf.validated_format().is_err()),
        }
    }

    #[test_case("<p>Hello <b>world</b></p>", "Hello world" ; "strips_tags")]
    #[test_case("<p>  hello   \n\t  world  </p>", "hello world" ; "normalizes_whitespace")]
    #[test_case("<script><script>inner</script></script><p>visible</p>", "visible" ; "nested_skip_blocks")]
    #[test_case(
        "<div>before</div><script>var x=1;</script><style>.a{}</style><noscript>no</noscript><p>after</p>",
        "before after" ; "skips_script_style_noscript"
    )]
    fn strip_html_tags_cases(input: &str, expected: &str) {
        assert_eq!(strip_html_tags(input), expected);
    }

    #[test_case("image/png", true ; "image_detected")]
    #[test_case("image/svg+xml", false ; "svg_allowed")]
    fn is_image_content_cases(ct: &str, expected: bool) {
        assert_eq!(is_image_content(ct), expected);
    }

    #[test_case(None,                "https://x.com"        ; "default_format")]
    #[test_case(Some("markdown"),     "https://x.com"        ; "markdown_hidden")]
    #[test_case(Some("text"),         "https://x.com [text]" ; "non_default_shown")]
    fn start_summary_cases(format: Option<&str>, expected: &str) {
        let wf = WebFetch {
            url: "https://x.com".into(),
            format: format.map(Into::into),
            timeout: None,
        };
        assert_eq!(wf.start_summary(), expected);
    }
}
