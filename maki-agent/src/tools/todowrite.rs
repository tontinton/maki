use crate::{TodoItem, ToolOutput};
use maki_tool_macro::Tool;
use serde::Deserialize;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct TodoWrite {
    #[param(description = "The updated todo list")]
    todos: Vec<TodoItem>,
}

impl TodoWrite {
    pub const NAME: &str = "todo_write";
    pub const DESCRIPTION: &str = include_str!("todowrite.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[{"todos": [{"content": "Add error handling", "status": "pending", "priority": "high"}]}]"#,
    );

    pub async fn execute(&self, _ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        Ok(ToolOutput::TodoList(self.todos.clone()))
    }
}

super::impl_tool!(TodoWrite, audience = super::ToolAudience::MAIN);

impl super::ToolInvocation for TodoWrite {
    fn start_summary(&self) -> super::SummaryFuture {
        super::SummaryFuture::Ready(format!("{} todos", self.todos.len()))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { TodoWrite::execute(&self, ctx).await })
    }
}
