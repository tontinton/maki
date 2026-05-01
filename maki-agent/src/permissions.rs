use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use maki_config::{
    Effect, PermissionRule, PermissionTarget, PermissionsConfig, append_permission_rule,
};
use thiserror::Error;
use tracing::{info, warn};

use crate::{AgentEvent, EventSender};

pub const DEFAULT_DENY_GUIDANCE: &str =
    "Do not retry. Try a different approach or ask the user for guidance.";

/// Tests assert on this exact prefix; a wording tweak here updates them in one place.
pub const PERMISSION_DENIED_PREFIX: &str = "Permission denied for";

fn builtin_rules(cwd: &Path) -> Vec<PermissionRule> {
    let cwd_glob = format!(
        "{}/**",
        cwd.canonicalize()
            .unwrap_or_else(|_| cwd.to_path_buf())
            .display()
    );
    let allow = |tool: &str, scope: &str| PermissionRule {
        tool: tool.into(),
        scope: Some(scope.into()),
        effect: Effect::Allow,
    };
    vec![
        allow("write", &cwd_glob),
        allow("edit", &cwd_glob),
        allow("multiedit", &cwd_glob),
        allow("task", "*"),
    ]
}

#[derive(Debug)]
pub enum PermissionCheck {
    Allowed,
    Denied,
    NeedsPrompt {
        tool: String,
        scopes: Vec<String>,
        force_prompt: bool,
    },
}

#[derive(Debug, Error)]
pub struct PermissionError {
    tool: String,
    scope: String,
    guidance: Option<String>,
}

impl std::fmt::Display for PermissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} `{}` ({}).",
            PERMISSION_DENIED_PREFIX, self.tool, self.scope
        )?;
        if let Some(g) = &self.guidance {
            write!(f, " User guidance: {}", g)
        } else {
            write!(f, " {}", DEFAULT_DENY_GUIDANCE)
        }
    }
}

impl PermissionError {
    fn new(tool: &str, scope: &str) -> Self {
        Self {
            tool: tool.to_string(),
            scope: scope.to_string(),
            guidance: None,
        }
    }

    fn with_guidance(tool: &str, scope: &str, guidance: String) -> Self {
        Self {
            tool: tool.to_string(),
            scope: scope.to_string(),
            guidance: Some(guidance),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionAnswer {
    AllowOnce,
    AllowSession,
    AllowAlwaysLocal,
    AllowAlwaysGlobal,
    Deny,
    DenyWithGuidance(String),
    DenyAlwaysLocal,
    DenyAlwaysGlobal,
}

impl PermissionAnswer {
    pub fn is_allow(&self) -> bool {
        matches!(
            self,
            Self::AllowOnce | Self::AllowSession | Self::AllowAlwaysLocal | Self::AllowAlwaysGlobal
        )
    }

    pub fn encode(&self) -> String {
        match self {
            Self::AllowOnce => "allow".to_string(),
            Self::AllowSession => "allow_session".to_string(),
            Self::AllowAlwaysLocal => "allow_always_local".to_string(),
            Self::AllowAlwaysGlobal => "allow_always_global".to_string(),
            Self::Deny => "deny".to_string(),
            Self::DenyWithGuidance(g) => format!("deny:{g}"),
            Self::DenyAlwaysLocal => "deny_always_local".to_string(),
            Self::DenyAlwaysGlobal => "deny_always_global".to_string(),
        }
    }

    pub fn decode(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(Self::AllowOnce),
            "allow_session" => Some(Self::AllowSession),
            "allow_always_local" => Some(Self::AllowAlwaysLocal),
            "allow_always_global" => Some(Self::AllowAlwaysGlobal),
            "deny" => Some(Self::Deny),
            "deny_always_local" => Some(Self::DenyAlwaysLocal),
            "deny_always_global" => Some(Self::DenyAlwaysGlobal),
            _ if s.starts_with("deny:") => {
                let guidance = s.strip_prefix("deny:").unwrap();
                if guidance.is_empty() {
                    Some(Self::Deny)
                } else {
                    Some(Self::DenyWithGuidance(guidance.to_string()))
                }
            }
            _ => None,
        }
    }

    pub fn guidance(&self) -> Option<&str> {
        match self {
            Self::DenyWithGuidance(g) => Some(g),
            _ => None,
        }
    }
}

pub struct PermissionManager {
    session_rules: Mutex<Vec<PermissionRule>>,
    config_rules: Vec<PermissionRule>,
    builtin_rules: Vec<PermissionRule>,
    allow_all: AtomicBool,
    cwd: PathBuf,
}

impl PermissionManager {
    pub fn new(config: PermissionsConfig, cwd: PathBuf) -> Self {
        Self {
            builtin_rules: builtin_rules(&cwd),
            session_rules: Mutex::new(Vec::new()),
            config_rules: config.rules,
            allow_all: AtomicBool::new(config.allow_all),
            cwd,
        }
    }

    fn session_rules(&self) -> std::sync::MutexGuard<'_, Vec<PermissionRule>> {
        self.session_rules.lock().unwrap_or_else(|e| {
            warn!("permission mutex was poisoned, recovering");
            e.into_inner()
        })
    }

    fn check_inner(&self, tool: &str, scopes: &[&str], force_prompt: bool) -> PermissionCheck {
        let session = self.session_rules();
        let rules = session
            .iter()
            .chain(&self.config_rules)
            .chain(&self.builtin_rules);

        for scope in scopes {
            for rule in rules.clone() {
                if rule.effect == Effect::Deny && matches_rule(rule, tool, scope) {
                    info!(tool, scope = %scope, "permission denied");
                    return PermissionCheck::Denied;
                }
            }
        }

        if self.allow_all.load(Ordering::Relaxed) {
            return PermissionCheck::Allowed;
        }

        let is_allowed = |scope: &&str| {
            rules
                .clone()
                .any(|rule| rule.effect == Effect::Allow && matches_rule(rule, tool, scope))
        };

        let pending: Vec<&str> = if force_prompt {
            scopes.to_vec()
        } else {
            scopes.iter().filter(|s| !is_allowed(s)).copied().collect()
        };

        if pending.is_empty() {
            return PermissionCheck::Allowed;
        }

        PermissionCheck::NeedsPrompt {
            tool: tool.to_string(),
            scopes: pending.into_iter().map(|s| s.to_string()).collect(),
            force_prompt,
        }
    }

    pub fn check(&self, tool: &str, scope: &str) -> PermissionCheck {
        self.check_inner(tool, &[scope], false)
    }

    pub fn check_multi(&self, tool: &str, scopes: &[&str], force_prompt: bool) -> PermissionCheck {
        self.check_inner(tool, scopes, force_prompt)
    }

    pub fn add_session_rule(&self, rule: PermissionRule) {
        let mut rules = self.session_rules();
        let exists = rules
            .iter()
            .any(|r| r.tool == rule.tool && r.scope == rule.scope && r.effect == rule.effect);
        if !exists {
            rules.push(rule);
        }
    }

    pub fn toggle_yolo(&self) -> bool {
        let prev = self.allow_all.fetch_xor(true, Ordering::Relaxed);
        !prev
    }

    pub fn is_yolo(&self) -> bool {
        self.allow_all.load(Ordering::Relaxed)
    }

    pub fn session_rules_snapshot(&self) -> Vec<PermissionRule> {
        self.session_rules().clone()
    }

    pub fn load_session_rules(&self, rules: Vec<PermissionRule>) {
        *self.session_rules() = rules;
    }

    pub fn apply_decision(&self, tool: &str, scopes: &[String], answer: &PermissionAnswer) {
        let resolved = if answer.is_allow() {
            generalized_scopes(tool, scopes)
        } else {
            scopes.to_vec()
        };

        match answer {
            PermissionAnswer::AllowOnce
            | PermissionAnswer::Deny
            | PermissionAnswer::DenyWithGuidance(_) => {}
            PermissionAnswer::AllowSession => {
                for s in &resolved {
                    self.add_session_rule(PermissionRule {
                        tool: tool.to_string(),
                        scope: Some(s.clone()),
                        effect: Effect::Allow,
                    });
                }
            }
            PermissionAnswer::AllowAlwaysLocal
            | PermissionAnswer::AllowAlwaysGlobal
            | PermissionAnswer::DenyAlwaysLocal
            | PermissionAnswer::DenyAlwaysGlobal => {
                let effect = if answer.is_allow() {
                    Effect::Allow
                } else {
                    Effect::Deny
                };
                let target = match answer {
                    PermissionAnswer::AllowAlwaysLocal | PermissionAnswer::DenyAlwaysLocal => {
                        PermissionTarget::Project(self.cwd.clone())
                    }
                    _ => PermissionTarget::Global,
                };
                for s in &resolved {
                    self.add_session_rule(PermissionRule {
                        tool: tool.to_string(),
                        scope: Some(s.clone()),
                        effect,
                    });
                    if let Err(e) = append_permission_rule(tool, Some(s), effect, &target) {
                        tracing::warn!(error = %e, "failed to persist permission rule");
                    }
                }
            }
        }
    }

    pub async fn enforce(
        &self,
        tool: &str,
        scopes: &crate::tools::PermissionScopes,
        event_tx: &EventSender,
        user_response_rx: Option<&async_lock::Mutex<flume::Receiver<String>>>,
        request_id: &str,
        cancel: &crate::CancelToken,
    ) -> Result<(), PermissionError> {
        let scope_refs: Vec<&str> = scopes.scopes.iter().map(|s| s.as_str()).collect();
        let deny = |guidance: Option<String>| {
            let display = scopes.scopes.join("; ");
            match guidance {
                Some(g) => PermissionError::with_guidance(tool, &display, g),
                None => PermissionError::new(tool, &display),
            }
        };

        let (pt, ps, force_prompt) = match self.check_inner(tool, &scope_refs, scopes.force_prompt)
        {
            PermissionCheck::Allowed => return Ok(()),
            PermissionCheck::Denied => return Err(deny(None)),
            PermissionCheck::NeedsPrompt {
                tool,
                scopes,
                force_prompt,
            } => (tool, scopes, force_prompt),
        };

        let Some(rx) = user_response_rx else {
            warn!(tool, scope = %scopes.scopes.join("; "), "no permission response channel");
            return Err(deny(None));
        };

        let guard = rx.lock().await;
        let refs: Vec<&str> = ps.iter().map(|s| s.as_str()).collect();
        let (t2, s2) = match self.check_inner(&pt, &refs, force_prompt) {
            PermissionCheck::Allowed => return Ok(()),
            PermissionCheck::Denied => return Err(deny(None)),
            PermissionCheck::NeedsPrompt { tool, scopes, .. } => (tool, scopes),
        };

        let _ = event_tx.send(AgentEvent::PermissionRequest {
            id: request_id.to_owned(),
            tool: t2.clone(),
            scopes: s2.clone(),
        });
        let response = cancel.race(guard.recv_async()).await;
        drop(guard);

        let answer = match response {
            Ok(Ok(a)) => a,
            Ok(Err(_)) => {
                warn!(tool, scope = %scopes.scopes.join("; "), "permission channel closed");
                return Err(deny(None));
            }
            Err(_) => return Err(deny(None)),
        };

        let Some(answer) = PermissionAnswer::decode(&answer) else {
            return Err(deny(None));
        };
        self.apply_decision(&t2, &s2, &answer);
        if answer.is_allow() {
            Ok(())
        } else {
            Err(deny(answer.guidance().map(String::from)))
        }
    }
}

fn matches_rule(rule: &PermissionRule, tool: &str, scope: &str) -> bool {
    let tool_matches = rule.tool == "*" || rule.tool == tool;
    if !tool_matches {
        return false;
    }
    match &rule.scope {
        None => true,
        Some(pattern) => scope_matches(pattern, scope),
    }
}

pub fn scope_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" || pattern == "**" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return value == prefix || value.starts_with(&format!("{prefix}/"));
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return value.starts_with(prefix);
    }
    pattern == value
}

pub fn canonicalize_scope_path(path: &str) -> String {
    let resolved = crate::tools::resolve_path(path).unwrap_or_else(|_| path.to_string());
    let p = Path::new(&resolved);
    match p.canonicalize() {
        Ok(abs) => abs.to_string_lossy().into_owned(),
        Err(_) => {
            let mut result = PathBuf::new();
            for component in p.components() {
                match component {
                    std::path::Component::ParentDir => {
                        result.pop();
                    }
                    std::path::Component::CurDir => {}
                    c => result.push(c),
                }
            }
            result.to_string_lossy().into_owned()
        }
    }
}

fn generalize_bash_segment(segment: &str) -> String {
    let first_token = segment.split_whitespace().next().unwrap_or(segment);
    format!("{first_token} *")
}

pub fn generalized_scopes(tool: &str, scopes: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    scopes
        .iter()
        .map(|s| generalize_scope(tool, s))
        .filter(|g| seen.insert(g.clone()))
        .collect()
}

fn generalize_scope(tool: &str, scope: &str) -> String {
    match tool {
        "bash" => generalize_bash_segment(scope),
        "write" | "edit" | "multiedit" => {
            let p = Path::new(scope);
            match p.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => {
                    format!("{}/**", parent.display())
                }
                _ => "**".to_string(),
            }
        }
        _ => scope.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn make_config(rules: Vec<PermissionRule>) -> PermissionsConfig {
        PermissionsConfig {
            allow_all: false,
            rules,
        }
    }

    fn allow_rule(scope: &str) -> PermissionRule {
        PermissionRule {
            tool: "bash".into(),
            scope: Some(scope.into()),
            effect: Effect::Allow,
        }
    }

    fn deny_rule(scope: &str) -> PermissionRule {
        PermissionRule {
            tool: "bash".into(),
            scope: Some(scope.into()),
            effect: Effect::Deny,
        }
    }

    fn default_mgr() -> PermissionManager {
        PermissionManager::new(PermissionsConfig::default(), PathBuf::from("/tmp"))
    }

    #[test_case("*", "anything" => true ; "star")]
    #[test_case("cargo *", "cargo test" => true ; "prefix")]
    #[test_case("cargo *", "git push" => false ; "prefix_no_match")]
    #[test_case("src/**", "src/main.rs" => true ; "glob")]
    #[test_case("src/**", "src/deep/nested/file.rs" => true ; "glob_deep_nested")]
    #[test_case("src/**", "src" => true ; "glob_exact_prefix")]
    #[test_case("src/**", "srcfoo" => false ; "glob_no_bare_prefix")]
    #[test_case("src/**", "other/src/main.rs" => false ; "glob_no_inner_match")]
    fn scope_match(pattern: &str, value: &str) -> bool {
        scope_matches(pattern, value)
    }

    #[test_case(vec!["cd /tmp", "cargo test"], vec!["cd *", "cargo *"], true ; "all_allowed")]
    #[test_case(vec!["cd /tmp", "cargo test"], vec!["cargo *"], false ; "missing_rule")]
    fn compound_check(scopes: Vec<&str>, rules: Vec<&str>, expect_allowed: bool) {
        let mgr = PermissionManager::new(
            make_config(rules.into_iter().map(allow_rule).collect()),
            PathBuf::from("/tmp"),
        );
        let check = mgr.check_multi("bash", &scopes, false);
        assert_eq!(matches!(check, PermissionCheck::Allowed), expect_allowed);
    }

    #[test]
    fn compound_denied_if_any_segment_denied() {
        let mgr = PermissionManager::new(
            make_config(vec![
                allow_rule("cd *"),
                allow_rule("cargo *"),
                deny_rule("rm *"),
            ]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check_multi("bash", &["cd /tmp", "cargo test", "rm -rf /"], false),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn complex_constructs_force_prompt_even_with_allow_star() {
        let mgr = PermissionManager::new(make_config(vec![allow_rule("*")]), PathBuf::from("/tmp"));
        assert!(matches!(
            mgr.check_multi("bash", &["echo $(whoami)"], true),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test_case("write", "/tmp/file.txt" => true ; "write_in_cwd")]
    #[test_case("write", "/etc/passwd" => false ; "write_outside_cwd")]
    #[test_case("task", "task:research" => true ; "task_allowed")]
    #[test_case("bash", "cargo test" => false ; "bash_prompts")]
    fn builtin_check(tool: &str, scope: &str) -> bool {
        matches!(default_mgr().check(tool, scope), PermissionCheck::Allowed)
    }

    #[test]
    fn path_traversal_prompts() {
        let path = canonicalize_scope_path("/tmp/../etc/passwd");
        assert!(matches!(
            default_mgr().check("write", &path),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn session_rule_overrides_config() {
        let mgr = PermissionManager::new(
            make_config(vec![allow_rule("cargo *")]),
            PathBuf::from("/tmp"),
        );
        mgr.add_session_rule(deny_rule("cargo *"));
        assert!(matches!(
            mgr.check("bash", "cargo test"),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn deny_overrides_allow_all() {
        let mgr = PermissionManager::new(
            PermissionsConfig {
                allow_all: true,
                rules: vec![deny_rule("rm *")],
            },
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "rm -rf /"),
            PermissionCheck::Denied
        ));
    }

    // When you allow "cargo test", we generalize to "cargo *" for convenience.
    // But denies stay exact, you probably have a good reason to block that specific thing.
    #[test]
    fn allow_decision_generalizes() {
        let mgr = default_mgr();
        mgr.apply_decision(
            "bash",
            &["cargo test --all".into()],
            &PermissionAnswer::AllowSession,
        );
        assert!(matches!(
            mgr.check("bash", "cargo build"),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn deny_decision_uses_exact() {
        let mgr = default_mgr();
        mgr.apply_decision(
            "bash",
            &["cargo test".into()],
            &PermissionAnswer::DenyAlwaysLocal,
        );
        assert!(matches!(
            mgr.check("bash", "cargo test"),
            PermissionCheck::Denied
        ));
        assert!(matches!(
            mgr.check("bash", "cargo build"),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn permission_answer_roundtrip() {
        for a in [
            PermissionAnswer::AllowOnce,
            PermissionAnswer::AllowSession,
            PermissionAnswer::AllowAlwaysLocal,
            PermissionAnswer::Deny,
            PermissionAnswer::DenyWithGuidance("hint".into()),
        ] {
            assert_eq!(PermissionAnswer::decode(&a.encode()), Some(a));
        }
    }

    #[test]
    fn check_multi_force_prompt_skips_allow_rules() {
        let mgr = PermissionManager::new(
            make_config(vec![allow_rule("cargo *"), allow_rule("git *")]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check_multi("bash", &["cargo test", "git push"], false),
            PermissionCheck::Allowed
        ));
        match mgr.check_multi("bash", &["cargo test", "git push"], true) {
            PermissionCheck::NeedsPrompt {
                scopes,
                force_prompt,
                ..
            } => {
                assert_eq!(scopes, vec!["cargo test", "git push"]);
                assert!(force_prompt);
            }
            other => panic!("expected NeedsPrompt, got {other:?}"),
        }
    }

    #[test]
    fn check_multi_deny_wins_over_force_prompt() {
        let mgr =
            PermissionManager::new(make_config(vec![deny_rule("rm *")]), PathBuf::from("/tmp"));
        assert!(matches!(
            mgr.check_multi("bash", &["rm -rf /"], true),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn check_multi_partial_coverage_prompts_uncovered() {
        let mgr = PermissionManager::new(
            make_config(vec![allow_rule("cargo *")]),
            PathBuf::from("/tmp"),
        );
        match mgr.check_multi("bash", &["cargo test", "git push", "ls"], false) {
            PermissionCheck::NeedsPrompt { scopes, .. } => {
                assert_eq!(scopes, vec!["git push", "ls"]);
            }
            other => panic!("expected NeedsPrompt, got {other:?}"),
        }
    }

    #[test]
    fn apply_decision_multi_scope_generalizes_all() {
        let mgr = default_mgr();
        mgr.apply_decision(
            "bash",
            &["cargo test".into(), "git status".into()],
            &PermissionAnswer::AllowSession,
        );
        assert!(matches!(
            mgr.check("bash", "cargo build"),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check("bash", "git push"),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn generalized_scopes_deduplicates() {
        let scopes = vec!["cargo test".into(), "cargo build".into()];
        let result = generalized_scopes("bash", &scopes);
        assert_eq!(result, vec!["cargo *"]);
    }

    #[test]
    fn generalized_scopes_preserves_distinct() {
        let scopes = vec!["cargo test".into(), "git status".into()];
        let result = generalized_scopes("bash", &scopes);
        assert_eq!(result, vec!["cargo *", "git *"]);
    }

    #[test_case("edit", "/home/user/project/src/main.rs" => "/home/user/project/src/**" ; "edit_uses_parent_dir")]
    #[test_case("edit", "/Cargo.toml" => "//**" ; "edit_root_file")]
    #[test_case("webfetch", "some:scope" => "some:scope" ; "unknown_tool_preserves_exact")]
    fn generalize_single_scope(tool: &str, scope: &str) -> String {
        generalized_scopes(tool, &[scope.into()])
            .into_iter()
            .next()
            .unwrap()
    }

    #[test]
    fn generalize_edit_scope_root_file() {
        let scopes = vec!["/Cargo.toml".into()];
        let result = generalized_scopes("edit", &scopes);
        assert_eq!(result, vec!["//**"]);
    }

    #[test]
    fn generalize_unknown_tool_preserves_exact() {
        let scopes = vec!["some:scope".into()];
        let result = generalized_scopes("webfetch", &scopes);
        assert_eq!(result, vec!["some:scope"]);
    }

    #[test]
    fn deny_rule_with_none_scope_blocks_everything() {
        let mgr = PermissionManager::new(
            make_config(vec![PermissionRule {
                tool: "bash".into(),
                scope: None,
                effect: Effect::Deny,
            }]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "anything"),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn wildcard_tool_rule_applies_cross_tool() {
        let mgr = PermissionManager::new(
            make_config(vec![PermissionRule {
                tool: "*".into(),
                scope: None,
                effect: Effect::Deny,
            }]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(mgr.check("bash", "ls"), PermissionCheck::Denied));
        assert!(matches!(
            mgr.check("write", "/tmp/x"),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn yolo_mode_allows_but_deny_still_blocks() {
        let mgr =
            PermissionManager::new(make_config(vec![deny_rule("rm *")]), PathBuf::from("/tmp"));
        mgr.toggle_yolo();
        assert!(mgr.is_yolo());
        assert!(matches!(
            mgr.check("bash", "cargo test"),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check("bash", "rm -rf /"),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn add_session_rule_is_idempotent() {
        let mgr = default_mgr();
        let rule = allow_rule("cargo *");
        mgr.add_session_rule(rule.clone());
        mgr.add_session_rule(rule.clone());
        mgr.add_session_rule(rule);
        assert_eq!(mgr.session_rules_snapshot().len(), 1);
    }

    #[test_case(PermissionAnswer::AllowOnce ; "allow_once")]
    #[test_case(PermissionAnswer::Deny ; "deny_once")]
    fn once_decisions_add_no_session_rules(answer: PermissionAnswer) {
        let mgr = default_mgr();
        mgr.apply_decision("bash", &["cargo test".into()], &answer);
        assert!(mgr.session_rules_snapshot().is_empty());
    }
}
