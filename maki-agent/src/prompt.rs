use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use strum::{Display, EnumIter, EnumString, IntoEnumIterator};

use crate::template::Vars;
use crate::tools::{ToolFilter, ToolRegistry};

pub trait ValidNames: IntoEnumIterator + std::fmt::Display {
    fn valid_names() -> String {
        Self::iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub const SYSTEM_PROMPT: &str = include_str!("prompts/system.md");
pub const PLAN_PROMPT: &str = include_str!("prompts/plan.md");
pub const RESEARCH_PROMPT: &str = include_str!("prompts/research.md");
pub const GENERAL_PROMPT: &str = include_str!("prompts/general.md");
pub const COMPACTION_SYSTEM: &str = include_str!("prompts/compaction.md");
pub const COMPACTION_USER: &str = include_str!("prompts/compaction_user.md");
pub const DEFAULT_IDENTITY: &str = include_str!("prompts/system_identity.md");
pub const DEFAULT_TONE: &str = include_str!("prompts/system_tone.md");

const DEFAULT_EFFICIENT_TOOLS: &str = include_str!("prompts/efficient_tools.md");
const INSTRUCTIONS_MARKER: &str = "instructions";
const INSTRUCTIONS_SLOT: &str = "{{instructions}}";

/// Singleton slots retain only the last contribution. Aggregate slots append
/// every contribution to the selected section default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, Display)]
#[strum(serialize_all = "snake_case")]
pub enum SlotKind {
    Singleton,
    Aggregate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, Display, EnumIter)]
#[strum(serialize_all = "snake_case")]
pub enum Slot {
    Identity,
    Tone,
    ToolUsage,
    EfficientTools,
    Conventions,
    AfterInstructions,
    Objectivity,
    Completion,
}

impl Slot {
    fn marker(self) -> &'static str {
        match self {
            Slot::Identity => "{{identity}}",
            Slot::Tone => "{{tone}}",
            Slot::ToolUsage => "{{tool_usage}}",
            Slot::EfficientTools => "{{efficient_tools}}",
            Slot::Conventions => "{{conventions}}",
            Slot::AfterInstructions => "{{after_instructions}}",
            Slot::Objectivity => "{{objectivity}}",
            Slot::Completion => "{{completion}}",
        }
    }

    pub fn kind(self) -> SlotKind {
        match self {
            Slot::Identity | Slot::Tone => SlotKind::Singleton,
            _ => SlotKind::Aggregate,
        }
    }

    pub fn names_for_kind(kind: SlotKind) -> String {
        Self::iter()
            .filter(|s| s.kind() == kind)
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, Display, EnumIter)]
#[strum(serialize_all = "snake_case")]
pub enum PromptId {
    System,
    Research,
    General,
    Plan,
}

impl PromptId {
    /// Default targets for additive hints without an explicit prompt list.
    pub const ALL: &[PromptId] = &[PromptId::System, PromptId::Research, PromptId::General];

    fn template(self) -> &'static str {
        match self {
            PromptId::System => SYSTEM_PROMPT,
            PromptId::Research => RESEARCH_PROMPT,
            PromptId::General => GENERAL_PROMPT,
            PromptId::Plan => PLAN_PROMPT,
        }
    }

    /// Whether a section is present in this prompt's built-in template.
    pub fn has_slot(self, slot: Slot) -> bool {
        self.template().contains(slot.marker())
    }

    pub(crate) fn default_section(self, slot: Slot) -> &'static str {
        match (self, slot) {
            (PromptId::System, Slot::Identity) => DEFAULT_IDENTITY,
            (PromptId::System, Slot::Tone) => DEFAULT_TONE,
            (PromptId::System, Slot::Objectivity) => include_str!("prompts/system_objectivity.md"),
            (PromptId::System, Slot::ToolUsage) => include_str!("prompts/system_tool_usage.md"),
            (PromptId::System, Slot::Conventions) => include_str!("prompts/system_conventions.md"),
            (PromptId::System, Slot::Completion) => include_str!("prompts/system_completion.md"),
            (PromptId::Research, Slot::Identity) => include_str!("prompts/research_identity.md"),
            (PromptId::Research, Slot::Tone) => include_str!("prompts/research_tone.md"),
            (PromptId::Research, Slot::ToolUsage) => include_str!("prompts/research_tool_usage.md"),
            (PromptId::Research, Slot::Completion) => {
                include_str!("prompts/research_completion.md")
            }
            (PromptId::General, Slot::Identity) => include_str!("prompts/general_identity.md"),
            (PromptId::General, Slot::Tone) => include_str!("prompts/general_tone.md"),
            (PromptId::General, Slot::ToolUsage) => include_str!("prompts/general_tool_usage.md"),
            (PromptId::General, Slot::Conventions) => {
                include_str!("prompts/general_conventions.md")
            }
            (PromptId::General, Slot::Completion) => include_str!("prompts/general_completion.md"),
            (_, Slot::EfficientTools) => DEFAULT_EFFICIENT_TOOLS,
            _ => "",
        }
    }
}

impl ValidNames for Slot {}
impl ValidNames for PromptId {}

#[derive(Clone)]
pub struct SlotEntry {
    pub plugin: Arc<str>,
    pub content: String,
}

#[derive(Clone, Default)]
pub struct ResolvedPrompts {
    entries: HashMap<(PromptId, Slot), Vec<SlotEntry>>,
    templates: HashMap<PromptId, String>,
    sections: HashMap<(PromptId, Slot), String>,
}

impl ResolvedPrompts {
    pub fn get(&self, prompt: PromptId, slot: Slot) -> &[SlotEntry] {
        self.entries
            .get(&(prompt, slot))
            .map(|v| v.as_slice())
            .unwrap_or_default()
    }

    pub fn insert(&mut self, prompt: PromptId, slot: Slot, entry: SlotEntry) {
        self.entries.entry((prompt, slot)).or_default().push(entry);
    }

    /// Without the hints of plugins whose every tool `filter` keeps out of the
    /// run, so the prompt never tells the model to call a tool it can't see.
    pub fn for_tools(&self, registry: &ToolRegistry, filter: &ToolFilter) -> Cow<'_, Self> {
        let hidden = registry.plugins_filtered_out(filter);
        if hidden.is_empty() {
            return Cow::Borrowed(self);
        }
        let mut prompts = self.clone();
        for entries in prompts.entries.values_mut() {
            entries.retain(|entry| !hidden.contains(&entry.plugin));
        }
        Cow::Owned(prompts)
    }

    pub fn set_template(&mut self, prompt: PromptId, content: String) {
        self.templates.insert(prompt, content);
    }

    pub fn set_section(&mut self, prompt: PromptId, slot: Slot, content: String) {
        self.sections.insert((prompt, slot), content);
    }

    pub fn has_template(&self, prompt: PromptId) -> bool {
        self.templates.contains_key(&prompt)
    }

    pub fn has_replacements(&self) -> bool {
        !self.templates.is_empty() || !self.sections.is_empty()
    }

    pub fn has_slot(&self, prompt: PromptId, slot: Slot) -> bool {
        self.template(prompt).contains(slot.marker())
    }

    pub fn renders_instructions(&self, prompt: PromptId) -> bool {
        self.template(prompt).contains(INSTRUCTIONS_SLOT)
    }

    pub fn template(&self, prompt: PromptId) -> &str {
        self.templates
            .get(&prompt)
            .map(String::as_str)
            .unwrap_or_else(|| prompt.template())
    }

    /// Static authored policy, in catalogue order. Lengths prevent text or
    /// delimiters in content from making distinct policies share a key.
    pub fn policy_key(&self) -> String {
        let mut key = String::new();
        for prompt in [PromptId::System, PromptId::Plan] {
            if let Some(content) = self.templates.get(&prompt) {
                key.push_str(&format!("template:{prompt}:{}:{content};", content.len()));
            }
            for slot in Slot::iter() {
                if let Some(content) = self.sections.get(&(prompt, slot)) {
                    key.push_str(&format!(
                        "section:{prompt}:{slot}:{}:{content};",
                        content.len()
                    ));
                }
            }
        }
        key
    }

    /// Expand only the template's variables and insertion points. Inserted
    /// plugin text and project instructions are never recursively rendered.
    pub fn render(&self, prompt: PromptId, vars: &Vars, instructions: &str) -> String {
        let mut remaining = self.template(prompt);
        let mut out = String::with_capacity(remaining.len());
        while let Some(open) = remaining.find("{{") {
            out.push_str(&vars.apply(&remaining[..open]));
            let marker = &remaining[open..];
            let Some(close) = marker.find("}}") else {
                out.push_str(&vars.apply(marker));
                return out;
            };
            let name = &marker[2..close];
            remaining = &marker[close + 2..];
            if name == INSTRUCTIONS_MARKER {
                out.push_str(instructions);
            } else if let Ok(slot) = name.parse::<Slot>() {
                let content = self.section(prompt, slot);
                out.push_str(&content);
                if content.is_empty() {
                    remaining = remaining.strip_prefix('\n').unwrap_or(remaining);
                }
            } else {
                out.push_str(&vars.apply(&marker[..close + 2]));
            }
        }
        out.push_str(&vars.apply(remaining));
        out
    }

    /// Effective section content, including its default and contributions.
    pub fn section(&self, prompt: PromptId, slot: Slot) -> String {
        let entries = self.get(prompt, slot);
        if slot.kind() == SlotKind::Singleton {
            return entries
                .last()
                .map(|entry| entry.content.as_str())
                .unwrap_or_else(|| prompt.default_section(slot))
                .to_string();
        }
        let replacement = self.sections.get(&(prompt, slot));
        let default = replacement
            .map(String::as_str)
            .unwrap_or_else(|| prompt.default_section(slot));
        if slot == Slot::EfficientTools && replacement.is_none() && !entries.is_empty() {
            let extras = entries
                .iter()
                .map(|entry| entry.content.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return format!("{}, {extras}.", default.trim_end_matches('.'));
        }
        std::iter::once(default)
            .chain(entries.iter().map(|entry| entry.content.as_str()))
            .filter(|content| !content.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Render without environment variables, for callers that supply none.
pub fn assemble(id: PromptId, prompts: &ResolvedPrompts, instructions: &str) -> String {
    prompts.render(id, &Vars::default(), instructions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    use crate::tools::test_support::mock_tool;
    use crate::tools::{ToolAudience, ToolSource};

    const NATIVE_EFFICIENT_LINE: &str = "Most efficient tools: batch, code_execution, task";

    fn slots(prompt: PromptId, entries: &[(Slot, &str)]) -> ResolvedPrompts {
        let mut slots = ResolvedPrompts::default();
        for &(slot, content) in entries {
            slots.insert(
                prompt,
                slot,
                SlotEntry {
                    plugin: Arc::from("p"),
                    content: content.into(),
                },
            );
        }
        slots
    }

    fn at(out: &str, needle: &str) -> usize {
        out.find(needle)
            .unwrap_or_else(|| panic!("missing: {needle}"))
    }

    #[test]
    fn empty_slots_emit_template_and_native_efficient_line() {
        let out = assemble(PromptId::System, &ResolvedPrompts::default(), "");
        assert!(out.starts_with("You are Maki"));
        assert!(
            !out.contains("{{"),
            "unfilled marker left in output:\n{out}"
        );
        assert!(out.contains(&format!("{NATIVE_EFFICIENT_LINE}.")));
    }

    /// One test to pin the whole System layout: every slot shows up, in order,
    /// around the instructions. Covers presence and ordering for all of them.
    #[test]
    fn system_sections_land_in_layout_order() {
        let s = slots(
            PromptId::System,
            &[
                (Slot::ToolUsage, "TOOL_USAGE"),
                (Slot::EfficientTools, "EXTRA_TOOL"),
                (Slot::Conventions, "CONVENTIONS"),
                (Slot::AfterInstructions, "AFTER"),
            ],
        );
        let out = assemble(PromptId::System, &s, "INSTR");
        let positions = ["TOOL_USAGE", "EXTRA_TOOL", "CONVENTIONS", "INSTR", "AFTER"]
            .map(|needle| at(&out, needle));
        assert!(
            positions.is_sorted(),
            "sections out of layout order ({positions:?}):\n{out}"
        );
    }

    const HINTED_PLUGIN: &str = "hinted";
    const TOOLLESS_PLUGIN: &str = "toolless";
    const TOOL_A: &str = "tool_a";
    const TOOL_B: &str = "tool_b";
    const HINT: &str = "Call tool_a first.";
    const TOOLLESS_HINT: &str = "Be brief.";

    #[test_case(ToolFilter::All, true ; "all_tools_sent")]
    #[test_case(ToolFilter::AllExcept(vec![TOOL_A.into()]), true ; "one_tool_still_sent")]
    #[test_case(ToolFilter::AllExcept(vec![TOOL_A.into(), TOOL_B.into()]), false ; "every_tool_excluded")]
    #[test_case(ToolFilter::Only(vec!["other".into()]), false ; "not_allowed")]
    fn hints_follow_their_plugins_tools(filter: ToolFilter, hinted: bool) {
        let registry = ToolRegistry::new();
        for name in [TOOL_A, TOOL_B] {
            let source = ToolSource::Lua {
                plugin: HINTED_PLUGIN.into(),
            };
            registry
                .register(mock_tool(name, ToolAudience::all()), source)
                .unwrap();
        }
        let mut prompts = ResolvedPrompts::default();
        for (plugin, content) in [(HINTED_PLUGIN, HINT), (TOOLLESS_PLUGIN, TOOLLESS_HINT)] {
            let entry = SlotEntry {
                plugin: plugin.into(),
                content: content.into(),
            };
            prompts.insert(PromptId::System, Slot::ToolUsage, entry);
        }

        let section = prompts
            .for_tools(&registry, &filter)
            .section(PromptId::System, Slot::ToolUsage);

        assert_eq!(section.contains(HINT), hinted);
        assert!(section.contains(TOOLLESS_HINT));
    }

    /// Regression: a `tool_usage` hint must land inside the `# Tool usage`
    /// section, not be appended after the rest of the prompt.
    #[test]
    fn tool_usage_hint_lands_inside_tool_usage_section() {
        const HINT: &str = "- HINT_LINE";
        let s = slots(PromptId::System, &[(Slot::ToolUsage, HINT)]);
        let out = assemble(PromptId::System, &s, "");
        let hint = at(&out, HINT);
        assert!(
            at(&out, "# Tool usage") < hint,
            "hint before its section:\n{out}"
        );
        assert!(
            hint < at(&out, "# Conventions"),
            "hint leaked past section:\n{out}"
        );
    }

    #[test]
    fn efficient_tools_extras_join_native_list() {
        let s = slots(
            PromptId::System,
            &[
                (Slot::EfficientTools, "index"),
                (Slot::EfficientTools, "foo"),
            ],
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains(&format!("{NATIVE_EFFICIENT_LINE}, index, foo.")));
    }

    #[test]
    fn same_slot_preserves_insertion_order() {
        let s = slots(
            PromptId::System,
            &[(Slot::ToolUsage, "FIRST"), (Slot::ToolUsage, "SECOND")],
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(at(&out, "FIRST") < at(&out, "SECOND"));
    }

    /// Only System carries AfterInstructions, so the same content shows up there
    /// but never leaks into the subagent prompts.
    #[test]
    fn after_instructions_only_reaches_system() {
        let mut s = ResolvedPrompts::default();
        for &pid in PromptId::ALL {
            s.insert(
                pid,
                Slot::AfterInstructions,
                SlotEntry {
                    plugin: Arc::from("p"),
                    content: "AFTER".into(),
                },
            );
        }
        assert!(assemble(PromptId::System, &s, "").contains("AFTER"));
        assert!(!assemble(PromptId::Research, &s, "").contains("AFTER"));
        assert!(!assemble(PromptId::General, &s, "").contains("AFTER"));
    }

    #[test]
    fn research_drops_conventions_but_keeps_efficient_extras() {
        let s = slots(
            PromptId::Research,
            &[
                (Slot::Conventions, "DROPPED"),
                (Slot::EfficientTools, "EXTRA"),
            ],
        );
        let out = assemble(PromptId::Research, &s, "");
        assert!(!out.contains("DROPPED"));
        assert!(out.contains(&format!("{NATIVE_EFFICIENT_LINE}, EXTRA.")));
    }

    #[test_case(PromptId::System, Slot::ToolUsage, true ; "system_tool_usage")]
    #[test_case(PromptId::System, Slot::EfficientTools, true ; "system_efficient")]
    #[test_case(PromptId::System, Slot::Conventions, true ; "system_conventions")]
    #[test_case(PromptId::System, Slot::AfterInstructions, true ; "system_after")]
    #[test_case(PromptId::System, Slot::Identity, true ; "system_identity")]
    #[test_case(PromptId::System, Slot::Tone, true ; "system_tone")]
    #[test_case(PromptId::Research, Slot::Conventions, false ; "research_no_conventions")]
    #[test_case(PromptId::Research, Slot::AfterInstructions, false ; "research_no_after")]
    #[test_case(PromptId::Research, Slot::Identity, true ; "research_identity")]
    #[test_case(PromptId::Research, Slot::Tone, true ; "research_tone")]
    #[test_case(PromptId::General, Slot::AfterInstructions, false ; "general_no_after")]
    #[test_case(PromptId::General, Slot::Identity, true ; "general_identity")]
    #[test_case(PromptId::General, Slot::Tone, true ; "general_tone")]
    fn has_slot(prompt: PromptId, slot: Slot, expected: bool) {
        assert_eq!(prompt.has_slot(slot), expected);
    }

    #[test_case("after_instructions", Some(Slot::AfterInstructions) ; "valid_slot")]
    #[test_case("tool_usagee", None ; "typo_slot")]
    #[test_case("identity", Some(Slot::Identity) ; "identity_slot")]
    #[test_case("tone", Some(Slot::Tone) ; "tone_slot")]
    fn slot_parse_is_plugin_contract(input: &str, expected: Option<Slot>) {
        assert_eq!(input.parse::<Slot>().ok(), expected);
    }

    #[test_case("system", Some(PromptId::System) ; "valid_prompt")]
    #[test_case("systm", None ; "typo_prompt")]
    fn prompt_parse_is_plugin_contract(input: &str, expected: Option<PromptId>) {
        assert_eq!(input.parse::<PromptId>().ok(), expected);
    }

    #[test_case(Slot::Identity, SlotKind::Singleton ; "identity_singleton")]
    #[test_case(Slot::Tone, SlotKind::Singleton ; "tone_singleton")]
    #[test_case(Slot::Conventions, SlotKind::Aggregate ; "conventions_aggregate")]
    #[test_case(Slot::ToolUsage, SlotKind::Aggregate ; "tool_usage_aggregate")]
    #[test_case(Slot::EfficientTools, SlotKind::Aggregate ; "efficient_aggregate")]
    #[test_case(Slot::AfterInstructions, SlotKind::Aggregate ; "after_aggregate")]
    fn slot_kind_matches_expectations(slot: Slot, expected: SlotKind) {
        assert_eq!(slot.kind(), expected);
    }

    #[test]
    fn singleton_default_used_when_empty() {
        let out = assemble(PromptId::System, &ResolvedPrompts::default(), "");
        assert!(out.starts_with("You are Maki"));
    }

    #[test]
    fn singleton_entry_replaces_default() {
        let mut s = ResolvedPrompts::default();
        s.insert(
            PromptId::System,
            Slot::Identity,
            SlotEntry {
                plugin: Arc::from("user"),
                content: "Custom identity".into(),
            },
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains("Custom identity"));
        assert!(!out.contains("You are Maki"));
    }

    #[test]
    fn singleton_last_entry_wins() {
        let mut s = ResolvedPrompts::default();
        s.insert(
            PromptId::System,
            Slot::Identity,
            SlotEntry {
                plugin: Arc::from("first"),
                content: "FIRST".into(),
            },
        );
        s.insert(
            PromptId::System,
            Slot::Identity,
            SlotEntry {
                plugin: Arc::from("second"),
                content: "SECOND".into(),
            },
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains("SECOND"));
        assert!(!out.contains("FIRST"));
        assert!(!out.contains("You are Maki"));
    }

    #[test]
    fn identity_is_overridable_in_all_conversational_prompts() {
        for &id in PromptId::ALL {
            assert!(id.has_slot(Slot::Identity));
        }
    }

    #[test]
    fn tone_is_overridable_in_all_conversational_prompts() {
        for &id in PromptId::ALL {
            assert!(id.has_slot(Slot::Tone));
        }
    }

    #[test]
    fn conventions_entry_appends_to_template_defaults() {
        let mut s = ResolvedPrompts::default();
        s.insert(
            PromptId::System,
            Slot::Conventions,
            SlotEntry {
                plugin: Arc::from("plugin"),
                content: "- Extra rule".into(),
            },
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains("Never assume a library is available"));
        assert!(out.contains("- Extra rule"));
    }

    #[test_case(PromptId::System, include_str!("prompts/tests/system.md") ; "system")]
    #[test_case(PromptId::Research, include_str!("prompts/tests/research.md") ; "research")]
    #[test_case(PromptId::General, include_str!("prompts/tests/general.md") ; "general")]
    #[test_case(PromptId::Plan, include_str!("prompts/tests/plan.md") ; "plan")]
    fn defaults_preserve_pre_catalogue_bytes(id: PromptId, expected: &str) {
        let vars = Vars::new()
            .set("{environment}", "Environment:\n- Working directory: /example\n- Platform: example\n- Date: 2000-01-01")
            .set("{plan_path}", "/example/plan.md");
        assert_eq!(
            ResolvedPrompts::default().render(id, &vars, "\n\nEXAMPLE_INSTRUCTIONS"),
            expected
        );
    }

    #[test]
    fn section_replacement_removes_default_and_preserves_additive_hints() {
        const REPLACEMENT: &str = "- Cite evidence.";
        const HINT: &str = "- Additional guidance.";
        let mut prompts = slots(PromptId::System, &[(Slot::Conventions, HINT)]);
        prompts.set_section(PromptId::System, Slot::Conventions, REPLACEMENT.into());
        let out = assemble(PromptId::System, &prompts, "");
        assert!(!out.contains("Never assume a library is available"));
        assert!(out.contains(&format!("{REPLACEMENT}\n{HINT}")));
    }

    #[test]
    fn efficient_tools_default_can_be_removed_without_dropping_hints() {
        const EXTRA: &str = "search";
        let mut prompts = slots(PromptId::System, &[(Slot::EfficientTools, EXTRA)]);
        prompts.set_section(PromptId::System, Slot::EfficientTools, String::new());
        let out = assemble(PromptId::System, &prompts, "");
        assert!(!out.contains(NATIVE_EFFICIENT_LINE));
        assert!(out.contains(&format!("\n{EXTRA}\n")));
    }

    #[test]
    fn empty_section_is_distinct_from_missing_section() {
        let mut prompts = ResolvedPrompts::default();
        prompts.set_section(PromptId::System, Slot::Conventions, String::new());
        let out = assemble(PromptId::System, &prompts, "");
        assert!(!out.contains("Never assume a library is available"));
        assert!(!out.starts_with("\n\n"));
        assert!(prompts.has_replacements());
        assert!(!prompts.policy_key().is_empty());
    }

    #[test]
    fn complete_template_contains_only_explicit_insertions() {
        const TEMPLATE: &str = "You analyse evidence.";
        let mut prompts = slots(PromptId::System, &[(Slot::ToolUsage, "IGNORED_HINT")]);
        prompts.set_template(PromptId::System, TEMPLATE.into());
        assert_eq!(
            assemble(PromptId::System, &prompts, "IGNORED_INSTRUCTIONS"),
            TEMPLATE
        );
        assert!(!prompts.has_slot(PromptId::System, Slot::ToolUsage));
        prompts.set_template(PromptId::System, String::new());
        assert!(prompts.has_template(PromptId::System));
        assert_eq!(
            assemble(PromptId::System, &prompts, "IGNORED_INSTRUCTIONS"),
            ""
        );
    }

    #[test]
    fn whole_and_section_replacements_compose_in_either_order() {
        const TEMPLATE: &str = "{{objectivity}}\n{{instructions}}";
        const SECTION: &str = "Follow the evidence.";
        let mut first = ResolvedPrompts::default();
        first.set_template(PromptId::Research, TEMPLATE.into());
        first.set_section(PromptId::Research, Slot::Objectivity, SECTION.into());
        let mut second = ResolvedPrompts::default();
        second.set_section(PromptId::Research, Slot::Objectivity, SECTION.into());
        second.set_template(PromptId::Research, TEMPLATE.into());
        assert!(first.has_slot(PromptId::Research, Slot::Objectivity));
        assert_eq!(
            assemble(PromptId::Research, &first, "INSTRUCTIONS"),
            format!("{SECTION}\nINSTRUCTIONS")
        );
        assert_eq!(first.policy_key(), second.policy_key());
        assert_eq!(
            assemble(PromptId::Research, &first, "INSTRUCTIONS"),
            assemble(PromptId::Research, &second, "INSTRUCTIONS")
        );
    }

    #[test]
    fn inserted_content_is_not_interpreted_as_template_text() {
        const SECTION: &str = "{environment} {{tone}} {{instructions}}";
        const INSTRUCTIONS: &str = "{environment} {{identity}}";
        let vars = Vars::new().set("{environment}", "ENVIRONMENT");
        let mut prompts = slots(PromptId::System, &[(Slot::ToolUsage, SECTION)]);
        prompts.set_template(
            PromptId::System,
            "{environment}\n{{tool_usage}}\n{{instructions}}".into(),
        );
        prompts.set_section(PromptId::System, Slot::ToolUsage, String::new());
        assert_eq!(
            prompts.render(PromptId::System, &vars, INSTRUCTIONS),
            format!("ENVIRONMENT\n{SECTION}\n{INSTRUCTIONS}")
        );
    }

    #[test]
    fn frame_policy_covers_only_system_and_plan_static_replacements() {
        let mut prompts = slots(PromptId::System, &[(Slot::Identity, "Legacy identity")]);
        assert!(prompts.policy_key().is_empty());
        for id in [PromptId::Research, PromptId::General] {
            prompts.set_template(id, String::new());
            prompts.set_section(id, Slot::Objectivity, String::new());
            assert!(prompts.policy_key().is_empty());
        }
        let mut keys = vec![prompts.policy_key()];
        for id in [PromptId::System, PromptId::Plan] {
            prompts.set_template(id, String::new());
            keys.push(prompts.policy_key());
            prompts.set_section(id, Slot::Objectivity, String::new());
            keys.push(prompts.policy_key());
        }
        for (index, key) in keys.iter().enumerate() {
            assert!(!keys[index + 1..].contains(key));
        }
    }

    #[test]
    fn variable_values_cannot_add_insertion_points() {
        const ENVIRONMENT: &str = "Working directory: {{identity}}";
        let vars = Vars::new().set("{environment}", ENVIRONMENT);
        let mut prompts = slots(PromptId::System, &[(Slot::Identity, "Selected identity")]);
        prompts.set_template(PromptId::System, "{environment}\n{{identity}}".into());
        assert_eq!(
            prompts.render(PromptId::System, &vars, ""),
            format!("{ENVIRONMENT}\nSelected identity")
        );
    }
}
