use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::Sender;

use tracing::info;

use crate::model::Model;
use crate::provider::Provider;
use crate::{
    AgentError, AgentEvent, AgentInput, AgentMode, Message, PendingToolCall, TokenUsage,
    ToolDoneEvent,
};

const AGENTS_MD: &str = "AGENTS.md";
const PLAN_MODE_LABEL: &str = "PLAN mode";

const SYSTEM_PROMPT_STATIC: &str = "\
You are Maki, a coding assistant. You help with software engineering tasks.
- Use tools to interact with the filesystem and execute commands
- Read files before editing them
- Be concise
- When done, summarize what you did";

pub fn build_system_prompt(cwd: &str, mode: &AgentMode) -> String {
    let mut prompt = SYSTEM_PROMPT_STATIC.to_string();
    prompt.push_str(&format!(
        "\n\nEnvironment:\n- Working directory: {cwd}\n- Platform: {}\n- Date: {}",
        env::consts::OS,
        current_date(),
    ));

    let agents_path = Path::new(cwd).join(AGENTS_MD);
    if let Ok(content) = fs::read_to_string(&agents_path) {
        prompt.push_str(&format!(
            "\n\nProject instructions ({AGENTS_MD}):\n{content}"
        ));
    }

    if let AgentMode::Plan(plan_path) = mode {
        prompt.push_str(&format!(
            "\n\nYou are in {PLAN_MODE_LABEL}. Do NOT make code changes. Only read, search, and analyze.\n\
             Write your plan to {plan_path}. When complete, tell the user."
        ));
    }

    prompt
}

fn current_date() -> String {
    let output = Command::new("date").arg("+%Y-%m-%d").output();
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => "unknown".to_string(),
    }
}

fn execute_tools(
    tool_calls: &[PendingToolCall],
    event_tx: &Sender<AgentEvent>,
    mode: &AgentMode,
) -> Vec<(String, ToolDoneEvent)> {
    std::thread::scope(|s| {
        let handles: Vec<_> = tool_calls
            .iter()
            .map(|pending| {
                let tx = event_tx.clone();
                s.spawn(move || {
                    let output = pending.call.execute(mode);
                    let _ = tx.send(AgentEvent::ToolDone(output.clone()));
                    output
                })
            })
            .collect();

        tool_calls
            .iter()
            .zip(handles)
            .map(|(pending, h)| {
                let output = h.join().unwrap_or_else(|_| ToolDoneEvent {
                    tool: "unknown",
                    content: "tool thread panicked".into(),
                    is_error: true,
                });
                (pending.id.clone(), output)
            })
            .collect()
    })
}

pub fn run(
    provider: &dyn Provider,
    model: &Model,
    input: AgentInput,
    history: &mut Vec<Message>,
    system: &str,
    event_tx: &Sender<AgentEvent>,
) -> Result<(), AgentError> {
    history.push(Message::user(input.effective_message()));
    let tools = crate::tool::ToolCall::definitions();
    let mut total_usage = TokenUsage::default();
    let mut num_turns: u32 = 0;

    loop {
        let response = provider.stream_message(model, history, system, &tools, event_tx)?;
        num_turns += 1;

        info!(
            input_tokens = response.usage.input,
            output_tokens = response.usage.output,
            cache_creation = response.usage.cache_creation,
            cache_read = response.usage.cache_read,
            tool_count = response.tool_calls.len(),
            "API response received"
        );

        event_tx.send(AgentEvent::TurnComplete {
            message: response.message.clone(),
            usage: response.usage.clone(),
            model: model.id.clone(),
        })?;

        total_usage += response.usage;
        history.push(response.message);

        if response.tool_calls.is_empty() {
            event_tx.send(AgentEvent::Done {
                usage: total_usage,
                num_turns,
                stop_reason: response.stop_reason,
            })?;
            break;
        }

        for pending in &response.tool_calls {
            event_tx.send(AgentEvent::ToolStart(pending.call.start_event()))?;
        }

        let tool_results = execute_tools(&response.tool_calls, event_tx, &input.mode);
        let tool_msg = Message::tool_results(tool_results);
        event_tx.send(AgentEvent::ToolResultsSubmitted {
            message: tool_msg.clone(),
        })?;
        history.push(tool_msg);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(&AgentMode::Build, false ; "build_excludes_plan")]
    #[test_case(&AgentMode::Plan(".maki/plans/123.md".into()), true ; "plan_includes_plan")]
    fn system_prompt_plan_section(mode: &AgentMode, expect_plan: bool) {
        let prompt = build_system_prompt("/tmp", mode);
        assert_eq!(prompt.contains(PLAN_MODE_LABEL), expect_plan);
    }
}
