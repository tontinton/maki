use serde::{Deserialize, Serialize};

use crate::ToolOutput;
use crate::skill::{Skill, build_skill_list_description};
use maki_tool_macro::Tool;

use super::ToolContext;

const NOT_FOUND: &str = "skill not found: ";

#[derive(Tool, Debug, Clone, Serialize, Deserialize)]
pub struct SkillTool {
    #[param(description = "Name of the skill to load")]
    name: String,
}

impl SkillTool {
    pub const NAME: &str = "skill";
    pub const DESCRIPTION: &str =
        "Load a skill that provides instructions and workflows for specific tasks.";
    pub const EXAMPLES: Option<&str> = Some(r#"[{"name": "rust-patterns"}]"#);

    pub async fn execute(&self, ctx: &ToolContext) -> Result<ToolOutput, String> {
        Skill::find(&self.name, &ctx.skills)
            .map(|s| s.to_tool_output())
            .ok_or_else(|| format!("{NOT_FOUND}{}", self.name))
    }

    pub fn start_summary(&self) -> String {
        self.name.clone()
    }
}

impl super::ToolDefaults for SkillTool {
    fn augment_description(description: &mut String, ctx: &super::DescriptionContext) {
        let desc = build_skill_list_description(ctx.skills);
        if !desc.is_empty() {
            description.push_str(&desc);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;

    fn test_skill() -> Skill {
        Skill {
            name: "test-skill".into(),
            description: "A test skill".into(),
            content: "Do the thing".into(),
            location: PathBuf::from("/home/.maki/skills/test-skill/SKILL.md"),
        }
    }

    #[test]
    fn execute_loads_skill_content() {
        smol::block_on(async {
            let skill = test_skill();
            let skills = [skill];
            let mut ctx = stub_ctx(&AgentMode::Build);
            ctx.skills = Arc::from(skills);

            let tool = SkillTool::parse_input(&json!({"name": "test-skill"})).unwrap();
            let output = tool.execute(&ctx).await.unwrap();
            assert!(output.as_text().contains("Do the thing"));
        });
    }

    #[test]
    fn execute_returns_error_when_not_found() {
        smol::block_on(async {
            let skills = [test_skill()];
            let mut ctx = stub_ctx(&AgentMode::Build);
            ctx.skills = Arc::from(skills);

            let tool = SkillTool::parse_input(&json!({"name": "nonexistent"})).unwrap();
            assert!(tool.execute(&ctx).await.unwrap_err().starts_with(NOT_FOUND));
        });
    }
}
