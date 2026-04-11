use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use tracing::info;

use std::collections::HashMap;

use super::error::McpError;
use super::protocol::{
    CallToolResult, GetPromptResult, PromptInfo, PromptsListResult, ToolInfo, ToolsListResult,
    initialize_params,
};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait McpTransport: Send + Sync {
    fn send_request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<Value, McpError>>;
    fn send_notification<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<(), McpError>>;
    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()>;
    fn server_name(&self) -> &Arc<str>;
    fn transport_kind(&self) -> &'static str;
    fn child_pids(&self) -> Vec<u32> {
        Vec::new()
    }
}

fn invalid_response(name: &Arc<str>, e: impl std::fmt::Display) -> McpError {
    McpError::InvalidResponse {
        server: (**name).into(),
        reason: e.to_string(),
    }
}

pub async fn initialize(transport: &dyn McpTransport) -> Result<(), McpError> {
    let params = initialize_params();
    transport.send_request("initialize", Some(params)).await?;
    transport
        .send_notification("notifications/initialized", None)
        .await
}

pub async fn list_tools(transport: &dyn McpTransport) -> Result<Vec<ToolInfo>, McpError> {
    let result = transport.send_request("tools/list", None).await?;
    let list: ToolsListResult =
        serde_json::from_value(result).map_err(|e| invalid_response(transport.server_name(), e))?;
    Ok(list.tools)
}

const METHOD_NOT_FOUND: i64 = -32601;

pub async fn list_prompts(transport: &dyn McpTransport) -> Result<Vec<PromptInfo>, McpError> {
    let result = transport.send_request("prompts/list", None).await;
    match result {
        Ok(val) => {
            let list: PromptsListResult = serde_json::from_value(val)
                .map_err(|e| invalid_response(transport.server_name(), e))?;
            Ok(list.prompts)
        }
        Err(McpError::RpcError { code, .. }) if code == METHOD_NOT_FOUND => Ok(vec![]),
        Err(e) => Err(e),
    }
}

pub async fn get_prompt(
    transport: &dyn McpTransport,
    name: &str,
    arguments: &HashMap<String, String>,
) -> Result<Vec<super::protocol::PromptMessage>, McpError> {
    let params = serde_json::json!({ "name": name, "arguments": arguments });
    let result = transport.send_request("prompts/get", Some(params)).await?;
    let parsed: GetPromptResult =
        serde_json::from_value(result).map_err(|e| invalid_response(transport.server_name(), e))?;
    Ok(parsed.messages)
}

pub async fn call_tool(
    transport: &dyn McpTransport,
    tool_name: &str,
    args: &Value,
) -> Result<String, McpError> {
    let server = &**transport.server_name();
    let start = Instant::now();
    let params = serde_json::json!({
        "name": tool_name,
        "arguments": args,
    });
    let result = transport.send_request("tools/call", Some(params)).await?;
    let call_result: CallToolResult =
        serde_json::from_value(result).map_err(|e| invalid_response(transport.server_name(), e))?;

    let text = call_result.joined_text();

    if call_result.is_error {
        return Err(McpError::RpcError {
            server: (**transport.server_name()).into(),
            code: -1,
            message: text,
        });
    }

    info!(
        server,
        tool = tool_name,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tools/call response"
    );
    Ok(text)
}
