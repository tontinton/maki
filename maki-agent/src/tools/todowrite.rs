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

    pub fn start_summary(&self) -> String {
        format!("{} todos", self.todos.len())
    }
}

impl super::ToolDefaults for TodoWrite {}
