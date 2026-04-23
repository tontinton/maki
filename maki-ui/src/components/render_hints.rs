use crate::markdown::Keep;
use maki_agent::RawRenderHints;
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
pub enum OutputSeparator {
    #[default]
    None,
    Bash,
    CodeExecution,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BodyFormat {
    #[default]
    Plain,
    Markdown,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToolRenderHints {
    pub header_style: HeaderStyle,
    pub body_format: BodyFormat,
    pub truncate_lines: Option<usize>,
    pub truncate_at: OutputKeep,
    pub output_separator: OutputSeparator,
}

impl Default for ToolRenderHints {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl ToolRenderHints {
    pub fn from_raw(raw: &RawRenderHints, existing: Option<&Self>) -> Self {
        Self {
            header_style: existing.map_or(HeaderStyle::Plain, |e| e.header_style),
            body_format: existing.map_or(BodyFormat::Plain, |e| e.body_format),
            truncate_lines: raw.truncate_lines,
            truncate_at: match raw.truncate_at.as_deref() {
                Some("tail") => OutputKeep::Tail,
                _ => OutputKeep::Head,
            },
            output_separator: match raw.output_separator.as_deref() {
                Some("bash") => OutputSeparator::Bash,
                Some("code_execution") => OutputSeparator::CodeExecution,
                _ => OutputSeparator::None,
            },
        }
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
        output_separator: OutputSeparator::None,
    };
}

const DEFAULT_HINTS: &[(&str, ToolRenderHints)] = &[
    hint!(BASH_TOOL_NAME,
        header_style: HeaderStyle::Command,
        truncate_at: OutputKeep::Tail,
        output_separator: OutputSeparator::Bash,
    ),
    hint!(CODE_EXECUTION_TOOL_NAME,
        truncate_at: OutputKeep::Tail,
        output_separator: OutputSeparator::CodeExecution,
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

    pub fn register(&mut self, name: Arc<str>, raw: &RawRenderHints) {
        let existing = self.hints.get(name.as_ref());
        let hints = ToolRenderHints::from_raw(raw, existing);
        self.hints.insert(name, hints);
    }

    pub fn get(&self, name: &str) -> &ToolRenderHints {
        static DEFAULT: ToolRenderHints = ToolRenderHints::DEFAULT;
        self.hints.get(name).unwrap_or(&DEFAULT)
    }

    #[allow(dead_code)]
    pub fn remove_plugin_tools(&mut self, tools: &[Arc<str>]) {
        for name in tools {
            self.hints.remove(name.as_ref());
        }
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

    #[test]
    fn register_and_remove_plugin_tool() {
        let mut reg = RenderHintsRegistry::new();
        let name: Arc<str> = Arc::from("my_plugin_tool");
        reg.register(Arc::clone(&name), &RawRenderHints::default());
        assert!(reg.hints.contains_key("my_plugin_tool"));
        reg.remove_plugin_tools(&[name]);
        assert!(!reg.hints.contains_key("my_plugin_tool"));
    }

    fn raw(f: impl FnOnce(&mut RawRenderHints)) -> ToolRenderHints {
        let mut r = RawRenderHints::default();
        f(&mut r);
        ToolRenderHints::from_raw(&r, None)
    }

    #[test_case("tail", OutputKeep::Tail ; "tail")]
    #[test_case("junk", OutputKeep::Head ; "invalid_falls_back_to_head")]
    fn from_raw_truncate_at(input: &str, expected: OutputKeep) {
        let h = raw(|r| r.truncate_at = Some(input.into()));
        assert_eq!(h.truncate_at, expected);
    }

    #[test_case("bash",           OutputSeparator::Bash          ; "bash")]
    #[test_case("code_execution", OutputSeparator::CodeExecution ; "code_execution")]
    #[test_case("invalid",        OutputSeparator::None          ; "invalid_falls_back_to_none")]
    fn from_raw_output_separator(input: &str, expected: OutputSeparator) {
        let h = raw(|r| r.output_separator = Some(input.into()));
        assert_eq!(h.output_separator, expected);
    }

    #[test]
    fn from_raw_never_sets_non_plain_body_format() {
        let h = raw(|r| {
            r.truncate_lines = Some(100);
            r.truncate_at = Some("tail".into());
            r.output_separator = Some("bash".into());
        });
        assert_eq!(h.body_format, BodyFormat::Plain);
    }

    #[test]
    fn register_preserves_existing_body_format() {
        let mut reg = RenderHintsRegistry::new();
        reg.register(Arc::from("custom"), &RawRenderHints::default());
        assert_eq!(reg.get("custom").body_format, BodyFormat::Plain);

        let mut entry = *reg.get("custom");
        entry.body_format = BodyFormat::Markdown;
        reg.hints.insert(Arc::from("custom"), entry);

        reg.register(
            Arc::from("custom"),
            &RawRenderHints {
                truncate_lines: Some(100),
                ..Default::default()
            },
        );
        assert_eq!(reg.get("custom").body_format, BodyFormat::Markdown);
    }

    #[test]
    fn from_raw_with_existing_markdown_preserves_it() {
        let existing = ToolRenderHints {
            body_format: BodyFormat::Markdown,
            ..Default::default()
        };
        let h = ToolRenderHints::from_raw(&RawRenderHints::default(), Some(&existing));
        assert_eq!(h.body_format, BodyFormat::Markdown);
    }

    #[test]
    fn register_new_tool_then_overwrite() {
        let mut reg = RenderHintsRegistry::new();
        let name: Arc<str> = Arc::from("custom_tool");
        reg.register(
            Arc::clone(&name),
            &RawRenderHints {
                truncate_lines: Some(50),
                ..Default::default()
            },
        );
        assert_eq!(reg.get("custom_tool").truncate_lines, Some(50));

        reg.register(
            Arc::clone(&name),
            &RawRenderHints {
                truncate_lines: Some(100),
                ..Default::default()
            },
        );
        assert_eq!(reg.get("custom_tool").truncate_lines, Some(100));
    }

    #[test]
    fn remove_plugin_tools_only_removes_specified() {
        let mut reg = RenderHintsRegistry::new();
        let a: Arc<str> = Arc::from("plugin_a");
        let b: Arc<str> = Arc::from("plugin_b");
        reg.register(Arc::clone(&a), &RawRenderHints::default());
        reg.register(Arc::clone(&b), &RawRenderHints::default());
        reg.remove_plugin_tools(&[a]);
        assert!(
            !reg.hints.contains_key("plugin_a"),
            "plugin_a should be removed"
        );
        assert!(reg.hints.contains_key("plugin_b"), "plugin_b should remain");
    }

    #[test]
    fn from_raw_all_defaults_match_const() {
        let h = ToolRenderHints::from_raw(&RawRenderHints::default(), None);
        assert_eq!(h.header_style, HeaderStyle::Plain);
        assert_eq!(h.body_format, BodyFormat::Plain);
        assert_eq!(h.truncate_lines, None);
        assert_eq!(h.truncate_at, OutputKeep::Head);
        assert_eq!(h.output_separator, OutputSeparator::None);
    }
}
