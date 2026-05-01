use crate::markdown::Keep;
use maki_agent::tools::{
    BASH_TOOL_NAME, BATCH_TOOL_NAME, CODE_EXECUTION_TOOL_NAME, EDIT_TOOL_NAME, GLOB_TOOL_NAME,
    GREP_TOOL_NAME, MEMORY_TOOL_NAME, MULTIEDIT_TOOL_NAME, QUESTION_TOOL_NAME, READ_TOOL_NAME,
    SKILL_TOOL_NAME, TASK_TOOL_NAME, TODOWRITE_TOOL_NAME, WRITE_TOOL_NAME,
};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HeaderStyle {
    #[default]
    Plain,
    Path,
    Command,
    Grep,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputKeep {
    #[default]
    Head,
    Tail,
}

impl From<OutputKeep> for Keep {
    fn from(k: OutputKeep) -> Self {
        match k {
            OutputKeep::Head => Keep::Head,
            OutputKeep::Tail => Keep::Tail,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BodyFormat {
    #[default]
    Plain,
    Markdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolRenderHints {
    pub header_style: HeaderStyle,
    pub body_format: BodyFormat,
    pub truncate_lines: Option<usize>,
    pub truncate_at: OutputKeep,
    pub input_code_field: Option<&'static str>,
    pub input_code_language: Option<&'static str>,
}

impl Default for ToolRenderHints {
    fn default() -> Self {
        Self::DEFAULT
    }
}

macro_rules! hint {
    ($name:expr $(, $field:ident : $val:expr)* $(,)?) => {
        ($name, {
            #[allow(unused_mut)]
            let mut h = ToolRenderHints::DEFAULT;
            $(h.$field = $val;)*
            h
        })
    };
}

impl ToolRenderHints {
    const DEFAULT: Self = Self {
        header_style: HeaderStyle::Plain,
        body_format: BodyFormat::Plain,
        truncate_lines: None,
        truncate_at: OutputKeep::Head,
        input_code_field: None,
        input_code_language: None,
    };
}

const DEFAULT_HINTS: &[(&str, ToolRenderHints)] = &[
    hint!(BASH_TOOL_NAME,
        header_style: HeaderStyle::Command,
        truncate_at: OutputKeep::Tail,
        input_code_field: Some("command"),
        input_code_language: Some("bash"),
    ),
    hint!(CODE_EXECUTION_TOOL_NAME,
        truncate_at: OutputKeep::Tail,
    ),
    hint!(TASK_TOOL_NAME,
        body_format: BodyFormat::Markdown,
    ),
    hint!(GREP_TOOL_NAME,
        header_style: HeaderStyle::Grep,
    ),
    hint!(GLOB_TOOL_NAME,
        header_style: HeaderStyle::Command,
    ),
    hint!(READ_TOOL_NAME, header_style: HeaderStyle::Path),
    hint!(WRITE_TOOL_NAME, header_style: HeaderStyle::Path),
    hint!(EDIT_TOOL_NAME, header_style: HeaderStyle::Path),
    hint!(MULTIEDIT_TOOL_NAME, header_style: HeaderStyle::Path),
    hint!(MEMORY_TOOL_NAME, header_style: HeaderStyle::Path),
    hint!(TODOWRITE_TOOL_NAME),
    hint!(QUESTION_TOOL_NAME),
    hint!(BATCH_TOOL_NAME),
    hint!(SKILL_TOOL_NAME),
];

pub struct RenderHintsRegistry {
    hints: HashMap<Arc<str>, ToolRenderHints>,
}

impl RenderHintsRegistry {
    pub fn new() -> Self {
        let hints = DEFAULT_HINTS
            .iter()
            .map(|(name, h)| (Arc::from(*name), *h))
            .collect();
        Self { hints }
    }

    pub fn get(&self, name: &str) -> &ToolRenderHints {
        static DEFAULT: ToolRenderHints = ToolRenderHints::DEFAULT;
        self.hints.get(name).unwrap_or(&DEFAULT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test]
    fn default_hints_are_known_tools() {
        let known: Vec<&str> = DEFAULT_HINTS.iter().map(|(n, _)| *n).collect();
        for name in &known {
            assert!(!name.is_empty(), "empty tool name in DEFAULT_HINTS");
        }
    }

    #[test]
    fn all_native_tools_have_hints() {
        let reg = RenderHintsRegistry::new();
        let native_names = maki_agent::tools::ToolRegistry::with_natives();
        for name in native_names.names() {
            assert!(
                reg.hints.contains_key(&*name),
                "native tool {name} missing from DEFAULT_HINTS"
            );
        }
    }

    #[test_case("bash", true ; "bash_tail_keep")]
    #[test_case("code_execution", true ; "code_execution_tail_keep")]
    #[test_case("read", false ; "read_head_keep")]
    fn truncate_at_direction(tool: &str, expect_tail: bool) {
        let reg = RenderHintsRegistry::new();
        let hints = reg.get(tool);
        assert_eq!(matches!(hints.truncate_at, OutputKeep::Tail), expect_tail);
    }

    #[test]
    fn unknown_tool_returns_default() {
        let reg = RenderHintsRegistry::new();
        let hints = reg.get("nonexistent_tool");
        assert!(matches!(hints.header_style, HeaderStyle::Plain));
    }
}
