use crate::markdown::Keep;
use maki_agent::RawRenderHints;
use maki_agent::tools::{
    BASH_TOOL_NAME, BATCH_TOOL_NAME, CODE_EXECUTION_TOOL_NAME, EDIT_TOOL_NAME,
    FIND_SYMBOL_TOOL_NAME, GLOB_TOOL_NAME, GREP_TOOL_NAME, INDEX_TOOL_NAME, MEMORY_TOOL_NAME,
    MULTIEDIT_TOOL_NAME, QUESTION_TOOL_NAME, READ_TOOL_NAME, SKILL_TOOL_NAME, TASK_TOOL_NAME,
    TODOWRITE_TOOL_NAME, WEBFETCH_TOOL_NAME, WEBSEARCH_TOOL_NAME, WRITE_TOOL_NAME,
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
    Index,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToolRenderHints {
    pub header_style: HeaderStyle,
    pub body_format: BodyFormat,
    pub output_lines: Option<usize>,
    pub output_keep: OutputKeep,
    pub output_separator: OutputSeparator,
    pub always_annotate: bool,
    pub skip_done_truncation: bool,
}

impl Default for ToolRenderHints {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl ToolRenderHints {
    pub fn from_raw(raw: &RawRenderHints, existing: Option<&Self>) -> Self {
        Self {
            header_style: match raw.header_style.as_deref() {
                Some("path") => HeaderStyle::Path,
                Some("command") => HeaderStyle::Command,
                Some("grep") => HeaderStyle::Grep,
                _ => HeaderStyle::Plain,
            },
            body_format: existing.map_or(BodyFormat::Plain, |e| e.body_format),
            output_lines: raw.output_lines,
            output_keep: match raw.output_keep.as_deref() {
                Some("tail") => OutputKeep::Tail,
                _ => OutputKeep::Head,
            },
            output_separator: match raw.output_separator.as_deref() {
                Some("bash") => OutputSeparator::Bash,
                Some("code_execution") => OutputSeparator::CodeExecution,
                _ => OutputSeparator::None,
            },
            always_annotate: raw.always_annotate.unwrap_or(false),
            skip_done_truncation: raw.skip_done_truncation.unwrap_or(false),
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
        output_lines: None,
        output_keep: OutputKeep::Head,
        output_separator: OutputSeparator::None,
        always_annotate: false,
        skip_done_truncation: false,
    };
}

const NATIVE_HINTS: &[(&str, ToolRenderHints)] = &[
    hint!(BASH_TOOL_NAME,
        header_style: HeaderStyle::Command,
        output_keep: OutputKeep::Tail,
        output_separator: OutputSeparator::Bash,
    ),
    hint!(CODE_EXECUTION_TOOL_NAME,
        output_keep: OutputKeep::Tail,
        output_separator: OutputSeparator::CodeExecution,
    ),
    hint!(TASK_TOOL_NAME,
        body_format: BodyFormat::Markdown,
    ),
    hint!(INDEX_TOOL_NAME,
        header_style: HeaderStyle::Path,
        body_format: BodyFormat::Index,
        always_annotate: true,
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
    hint!(WEBFETCH_TOOL_NAME,
        always_annotate: true,
        skip_done_truncation: true,
    ),
    hint!(WEBSEARCH_TOOL_NAME,
        always_annotate: true,
    ),
    hint!(FIND_SYMBOL_TOOL_NAME),
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
        let hints = NATIVE_HINTS
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
    use maki_agent::tools::native_static_name;
    use test_case::test_case;

    #[test]
    fn native_hints_complete() {
        for (name, _) in NATIVE_HINTS {
            assert!(
                native_static_name(name).is_some(),
                "{name} in NATIVE_HINTS is not a registered native tool"
            );
        }
    }

    #[test]
    fn all_native_tools_have_hints() {
        let reg = RenderHintsRegistry::new();
        let native_names = maki_agent::tools::ToolRegistry::with_natives();
        for name in native_names.names() {
            assert!(
                reg.hints.contains_key(&*name),
                "native tool {name} missing from NATIVE_HINTS"
            );
        }
    }

    #[test_case("bash", true ; "bash_tail_keep")]
    #[test_case("code_execution", true ; "code_execution_tail_keep")]
    #[test_case("read", false ; "read_head_keep")]
    fn output_keep_direction(tool: &str, expect_tail: bool) {
        let reg = RenderHintsRegistry::new();
        let hints = reg.get(tool);
        assert_eq!(matches!(hints.output_keep, OutputKeep::Tail), expect_tail);
    }

    #[test_case("webfetch", true ; "webfetch_always_annotate")]
    #[test_case("websearch", true ; "websearch_always_annotate")]
    #[test_case("index", true ; "index_always_annotate")]
    #[test_case("bash", false ; "bash_no_always_annotate")]
    fn always_annotate(tool: &str, expected: bool) {
        let reg = RenderHintsRegistry::new();
        assert_eq!(reg.get(tool).always_annotate, expected);
    }

    #[test]
    fn unknown_tool_returns_default() {
        let reg = RenderHintsRegistry::new();
        let hints = reg.get("nonexistent_tool");
        assert!(matches!(hints.header_style, HeaderStyle::Plain));
        assert!(!hints.always_annotate);
    }

    #[test]
    fn register_and_remove_plugin_tool() {
        let mut reg = RenderHintsRegistry::new();
        let name: Arc<str> = Arc::from("my_plugin_tool");
        reg.register(
            Arc::clone(&name),
            &RawRenderHints {
                always_annotate: Some(true),
                ..Default::default()
            },
        );
        assert!(reg.get("my_plugin_tool").always_annotate);
        reg.remove_plugin_tools(&[name]);
        assert!(!reg.get("my_plugin_tool").always_annotate);
    }

    #[test_case("webfetch", true ; "webfetch_skip_done")]
    #[test_case("bash", false ; "bash_no_skip_done")]
    fn skip_done_truncation(tool: &str, expected: bool) {
        let reg = RenderHintsRegistry::new();
        assert_eq!(reg.get(tool).skip_done_truncation, expected);
    }

    fn raw(f: impl FnOnce(&mut RawRenderHints)) -> ToolRenderHints {
        let mut r = RawRenderHints::default();
        f(&mut r);
        ToolRenderHints::from_raw(&r, None)
    }

    #[test_case("path",    HeaderStyle::Path    ; "path")]
    #[test_case("command", HeaderStyle::Command ; "command")]
    #[test_case("grep",    HeaderStyle::Grep    ; "grep")]
    #[test_case("unknown", HeaderStyle::Plain   ; "unknown_falls_back_to_plain")]
    fn from_raw_header_style(input: &str, expected: HeaderStyle) {
        let h = raw(|r| r.header_style = Some(input.into()));
        assert_eq!(h.header_style, expected);
    }

    #[test_case("tail", OutputKeep::Tail ; "tail")]
    #[test_case("junk", OutputKeep::Head ; "invalid_falls_back_to_head")]
    fn from_raw_output_keep(input: &str, expected: OutputKeep) {
        let h = raw(|r| r.output_keep = Some(input.into()));
        assert_eq!(h.output_keep, expected);
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
            r.header_style = Some("path".into());
            r.output_lines = Some(100);
            r.output_keep = Some("tail".into());
            r.output_separator = Some("bash".into());
            r.always_annotate = Some(true);
        });
        assert_eq!(h.body_format, BodyFormat::Plain);
    }

    #[test]
    fn register_preserves_native_body_format() {
        let mut reg = RenderHintsRegistry::new();
        assert_eq!(reg.get(INDEX_TOOL_NAME).body_format, BodyFormat::Index);
        reg.register(
            Arc::from(INDEX_TOOL_NAME),
            &RawRenderHints {
                header_style: Some("path".into()),
                always_annotate: Some(true),
                ..Default::default()
            },
        );
        assert_eq!(reg.get(INDEX_TOOL_NAME).body_format, BodyFormat::Index);
        assert!(reg.get(INDEX_TOOL_NAME).always_annotate);
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
                header_style: Some("command".into()),
                output_lines: Some(50),
                ..Default::default()
            },
        );
        assert_eq!(reg.get("custom_tool").header_style, HeaderStyle::Command);
        assert_eq!(reg.get("custom_tool").output_lines, Some(50));

        reg.register(
            Arc::clone(&name),
            &RawRenderHints {
                header_style: Some("path".into()),
                output_lines: Some(100),
                ..Default::default()
            },
        );
        assert_eq!(reg.get("custom_tool").header_style, HeaderStyle::Path);
        assert_eq!(reg.get("custom_tool").output_lines, Some(100));
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
        assert_eq!(h.output_lines, None);
        assert_eq!(h.output_keep, OutputKeep::Head);
        assert_eq!(h.output_separator, OutputSeparator::None);
        assert!(!h.always_annotate);
        assert!(!h.skip_done_truncation);
    }
}
