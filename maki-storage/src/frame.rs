//! The prefix every request of a session shares. Anthropic binds each thinking
//! block to the `system`, `tools` and messages before it, and any provider
//! with prefix caching pays a full miss when they change. So a session keeps
//! one frame, and later changes are told to the model instead of edited in.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a rendered system prompt states that can change later, kept as data
/// so a newer value can be spotted and told as an update.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptFacts {
    pub date: String,
    pub cwd: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub instructions: String,
    /// Prompt hint content keyed by `plugin/slot`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hints: BTreeMap<String, String>,
}

/// What the model holds true: its frame's facts with every update since
/// applied in transcript order, exactly as it read them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveFacts {
    /// `Some` in plan mode. Never part of a rendered system prompt, so every
    /// frame starts without it and entering plan mode is an update.
    pub plan: Option<PathBuf>,
    /// `None` under a prompt the host wrote. Which facts it states is
    /// unknown, so none of them are tracked.
    pub prompt: Option<PromptFacts>,
}

/// A new variant is not skew-safe: an older build drops the whole message
/// holding it, so it needs a `LOG_FORMAT_VERSION` bump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanChange {
    Entered(PathBuf),
    Ended,
}

/// What one context update told the model: only the facts that changed, so
/// an update weighs what it says and not the whole prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactsUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanChange>,
    /// `None` for a hint that no longer applies.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hints: BTreeMap<String, Option<String>>,
}

impl FactsUpdate {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl LiveFacts {
    pub fn apply(&mut self, update: &FactsUpdate) {
        match &update.plan {
            Some(PlanChange::Entered(path)) => self.plan = Some(path.clone()),
            Some(PlanChange::Ended) => self.plan = None,
            None => {}
        }
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        for (field, value) in [
            (&mut prompt.date, &update.date),
            (&mut prompt.model, &update.model),
            (&mut prompt.cwd, &update.cwd),
            (&mut prompt.instructions, &update.instructions),
        ] {
            if let Some(value) = value {
                value.clone_into(field);
            }
        }
        for (key, content) in &update.hints {
            match content {
                Some(content) => prompt.hints.insert(key.clone(), content.clone()),
                None => prompt.hints.remove(key),
            };
        }
    }

    /// What `self` has to hear to hold `now`. Prompt facts are only compared
    /// when both sides track them.
    pub fn changes_to(&self, now: &LiveFacts) -> FactsUpdate {
        let mut update = FactsUpdate {
            plan: (self.plan != now.plan).then(|| match &now.plan {
                Some(path) => PlanChange::Entered(path.clone()),
                None => PlanChange::Ended,
            }),
            ..FactsUpdate::default()
        };
        let (Some(told), Some(now)) = (&self.prompt, &now.prompt) else {
            return update;
        };
        let changed = |told: &String, now: &String| (told != now).then(|| now.clone());
        update.date = changed(&told.date, &now.date);
        update.model = changed(&told.model, &now.model);
        update.cwd = changed(&told.cwd, &now.cwd);
        update.instructions = changed(&told.instructions, &now.instructions);
        for (key, content) in &now.hints {
            if told.hints.get(key) != Some(content) {
                update.hints.insert(key.clone(), Some(content.clone()));
            }
        }
        for key in told.hints.keys().filter(|k| !now.hints.contains_key(*k)) {
            update.hints.insert(key.clone(), None);
        }
        update
    }
}

/// Base tools are kept as part of a fingerprint only. They are rebuilt from
/// the registry on every run, and a stored schema could resurrect a tool a
/// newer plugin set dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredFrame {
    pub system: String,
    /// A hash of what no update can carry. A run whose fingerprint differs
    /// starts a new frame.
    pub fingerprint: String,
    /// What `system` says. `None` when the host wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<PromptFacts>,
    /// The MCP part of `tools`, exactly as sent and grown as servers connect.
    /// Kept whole because what it holds depends on connection timing that a
    /// resumed process cannot reproduce.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_tools: Vec<Value>,
}

impl StoredFrame {
    /// What the model holds true before any update.
    pub fn facts_at_start(&self) -> LiveFacts {
        LiveFacts {
            plan: None,
            prompt: self.facts.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const HINT: &str = "memory/after_instructions";
    const PLAN_PATH: &str = "/plans/p.md";

    fn prompt() -> PromptFacts {
        PromptFacts {
            date: "2026-10-02".into(),
            cwd: "/repo".into(),
            model: "anthropic/claude-opus-5-5".into(),
            instructions: "be nice".into(),
            hints: BTreeMap::from([(HINT.into(), "tags: a".into())]),
        }
    }

    fn rendered() -> LiveFacts {
        LiveFacts {
            plan: None,
            prompt: Some(prompt()),
        }
    }

    fn host_written() -> LiveFacts {
        LiveFacts::default()
    }

    #[test_case(|f: &mut LiveFacts| f.plan = Some(PLAN_PATH.into()) ; "plan_entered")]
    #[test_case(|f: &mut LiveFacts| f.prompt.as_mut().unwrap().date = "2026-10-03".into() ; "date")]
    #[test_case(|f: &mut LiveFacts| f.prompt.as_mut().unwrap().instructions.clear() ; "instructions_gone")]
    #[test_case(|f: &mut LiveFacts| { f.prompt.as_mut().unwrap().hints.insert(HINT.into(), "tags: b".into()); } ; "hint_changed")]
    #[test_case(|f: &mut LiveFacts| f.prompt.as_mut().unwrap().hints.clear() ; "hint_removed")]
    fn applying_the_changes_reaches_now(change: fn(&mut LiveFacts)) {
        let told = rendered();
        let mut now = rendered();
        change(&mut now);

        let update = told.changes_to(&now);
        assert!(!update.is_empty());
        let mut heard = told;
        heard.apply(&update);
        assert_eq!(heard, now);
        assert!(heard.changes_to(&now).is_empty());
    }

    /// Only a rendered prompt says which facts it states, so swapping it for
    /// one the host wrote, either way, must not tell a fact as blank or
    /// repeat what the new prompt already says.
    #[test_case(rendered(), host_written() ; "rendered_to_host_written")]
    #[test_case(host_written(), rendered() ; "host_written_to_rendered")]
    fn untracked_prompt_facts_tell_nothing(told: LiveFacts, now: LiveFacts) {
        assert!(told.changes_to(&now).is_empty());
    }

    #[test]
    fn plan_is_tracked_under_any_prompt() {
        let now = LiveFacts {
            plan: Some(PLAN_PATH.into()),
            prompt: None,
        };
        assert_eq!(
            rendered().changes_to(&now),
            FactsUpdate {
                plan: Some(PlanChange::Entered(PLAN_PATH.into())),
                ..FactsUpdate::default()
            }
        );
    }
}
