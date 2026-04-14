use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use maki_config::{
    Effect, PermissionRule, PermissionTarget, PermissionsConfig, append_permission_rule,
};
use thiserror::Error;
use tracing::{info, warn};
use tree_sitter::{Node, Parser};

use crate::{AgentEvent, EventSender};

const COMPLEX_NODE_TYPES: &[&str] = &[
    "command_substitution",
    "process_substitution",
    "subshell",
    "arithmetic_expansion",
];

pub const DEFAULT_DENY_GUIDANCE: &str =
    "Do not retry. Try a different approach or ask the user for guidance.";

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

thread_local! {
    static BASH_PARSER: RefCell<Parser> = RefCell::new({
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_bash::LANGUAGE.into()).expect("failed to load bash grammar");
        parser
    });
}

fn parse_bash(input: &str) -> Option<tree_sitter::Tree> {
    BASH_PARSER.with(|p| p.borrow_mut().parse(input, None))
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
        write!(f, "Permission denied for `{}` ({}).", self.tool, self.scope)?;
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

    fn check_bash(&self, command: &str) -> PermissionCheck {
        let (scopes, is_complex) = analyze_bash(command);
        let scope_refs: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();
        self.check_inner("bash", &scope_refs, is_complex)
    }

    pub fn check(&self, tool: &str, scope: &str) -> PermissionCheck {
        if tool == "bash" {
            return self.check_bash(scope);
        }
        self.check_inner(tool, &[scope], false)
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
        scope: &str,
        event_tx: &EventSender,
        user_response_rx: Option<&async_lock::Mutex<flume::Receiver<String>>>,
        request_id: &str,
        cancel: &crate::CancelToken,
    ) -> Result<(), PermissionError> {
        match self.check(tool, scope) {
            PermissionCheck::Allowed => Ok(()),
            PermissionCheck::Denied => Err(PermissionError::new(tool, scope)),
            PermissionCheck::NeedsPrompt {
                tool: pt,
                scopes: ps,
                force_prompt,
            } => {
                let Some(rx) = user_response_rx else {
                    warn!(tool, scope, "no permission response channel");
                    return Err(PermissionError::new(tool, scope));
                };
                let guard = rx.lock().await;
                let refs: Vec<&str> = ps.iter().map(|s| s.as_str()).collect();
                match self.check_inner(&pt, &refs, force_prompt) {
                    PermissionCheck::Allowed => {
                        drop(guard);
                        Ok(())
                    }
                    PermissionCheck::Denied => {
                        drop(guard);
                        Err(PermissionError::new(tool, scope))
                    }
                    PermissionCheck::NeedsPrompt {
                        tool: t2,
                        scopes: s2,
                        ..
                    } => {
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
                                warn!(tool, scope, "permission channel closed");
                                return Err(PermissionError::new(tool, scope));
                            }
                            Err(_) => return Err(PermissionError::new(tool, scope)),
                        };
                        match PermissionAnswer::decode(&answer) {
                            Some(a) => {
                                self.apply_decision(&t2, &s2, &a);
                                if a.is_allow() {
                                    Ok(())
                                } else if let Some(guidance) = a.guidance() {
                                    Err(PermissionError::with_guidance(
                                        tool,
                                        scope,
                                        guidance.to_string(),
                                    ))
                                } else {
                                    Err(PermissionError::new(tool, scope))
                                }
                            }
                            None => Err(PermissionError::new(tool, scope)),
                        }
                    }
                }
            }
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

pub fn split_shell_commands(input: &str) -> Vec<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let Some(tree) = parse_bash(trimmed) else {
        return vec![trimmed.to_string()];
    };
    let mut segments = Vec::new();
    collect_commands(tree.root_node(), trimmed, &mut segments);
    if segments.is_empty() {
        vec![trimmed.to_string()]
    } else {
        segments
    }
}

fn collect_commands(node: Node, source: &str, out: &mut Vec<String>) {
    match node.kind() {
        "program" | "list" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_commands(child, source, out);
            }
        }
        "pipeline" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.is_named() {
                    let text = &source[child.start_byte()..child.end_byte()];
                    let text = text.trim();
                    if !text.is_empty() {
                        out.push(text.to_string());
                    }
                }
            }
        }
        "command"
        | "redirected_statement"
        | "negated_command"
        | "subshell"
        | "compound_statement"
        | "if_statement"
        | "while_statement"
        | "for_statement"
        | "case_statement"
        | "function_definition"
        | "c_style_for_statement" => {
            let text = &source[node.start_byte()..node.end_byte()];
            let text = text.trim();
            if !text.is_empty() {
                out.push(text.to_string());
            }
        }
        kind if node.is_named() => {
            warn!(
                node_kind = kind,
                "unknown bash AST node in permission check"
            );
        }
        _ => {}
    }
}

fn analyze_bash(command: &str) -> (Vec<String>, bool) {
    let Some(tree) = parse_bash(command) else {
        return (vec![command.to_string()], true);
    };
    if is_complex_bash(&tree) {
        return (vec![command.to_string()], true);
    }
    let mut segments = Vec::new();
    collect_commands(tree.root_node(), command, &mut segments);
    if segments.is_empty() {
        (vec![command.to_string()], false)
    } else {
        (segments, false)
    }
}

fn is_complex_bash(tree: &tree_sitter::Tree) -> bool {
    has_complex_node(tree.root_node()) || has_error_node(tree.root_node())
}

fn has_complex_node(node: Node) -> bool {
    if COMPLEX_NODE_TYPES.contains(&node.kind()) {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|c| has_complex_node(c))
}

fn has_error_node(node: Node) -> bool {
    if node.is_error() || node.is_missing() {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|c| has_error_node(c))
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
        "webfetch" | "websearch" => "*".to_string(),
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

    #[test_case("cargo test" => vec!["cargo test"] ; "single")]
    #[test_case("a && b || c ; d" => vec!["a", "b", "c", "d"] ; "operators")]
    #[test_case("a | b" => vec!["a", "b"] ; "pipe")]
    #[test_case("echo \"a && b\"" => vec!["echo \"a && b\""] ; "quoted_not_split")]
    #[test_case("" => Vec::<String>::new() ; "empty")]
    fn split_shell(input: &str) -> Vec<String> {
        split_shell_commands(input)
    }

    #[test_case("*", "anything" => true ; "star")]
    #[test_case("cargo *", "cargo test" => true ; "prefix")]
    #[test_case("cargo *", "git push" => false ; "prefix_no_match")]
    #[test_case("src/**", "src/main.rs" => true ; "glob")]
    #[test_case("src/**", "srcfoo" => false ; "glob_no_bare_prefix")]
    fn scope_match(pattern: &str, value: &str) -> bool {
        scope_matches(pattern, value)
    }

    // Things like $(cmd) or subshells can hide dangerous commands, so we always ask
    #[test_case("cargo test" => false ; "simple")]
    #[test_case("echo $(whoami)" => true ; "command_sub")]
    #[test_case("(cd /tmp && ls)" => true ; "subshell")]
    #[test_case("echo 'safe $(x)'" => false ; "single_quoted_safe")]
    #[test_case("echo $(((" => true ; "parse_error")]
    fn complex_shell(input: &str) -> bool {
        analyze_bash(input).1
    }

    #[test_case("cd /tmp && cargo test", vec!["cd *", "cargo *"], true ; "all_allowed")]
    #[test_case("cd /tmp && cargo test", vec!["cargo *"], false ; "missing_rule")]
    fn compound_check(cmd: &str, rules: Vec<&str>, expect_allowed: bool) {
        let mgr = PermissionManager::new(
            make_config(rules.into_iter().map(allow_rule).collect()),
            PathBuf::from("/tmp"),
        );
        let check = mgr.check("bash", cmd);
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
            mgr.check("bash", "cd /tmp && cargo test && rm -rf /"),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn complex_constructs_force_prompt_even_with_allow_star() {
        let mgr = PermissionManager::new(make_config(vec![allow_rule("*")]), PathBuf::from("/tmp"));
        assert!(matches!(
            mgr.check("bash", "echo $(whoami)"),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test_case("write", "/tmp/file.txt" => true ; "write_in_cwd")]
    #[test_case("write", "/etc/passwd" => false ; "write_outside_cwd")]
    #[test_case("task", "task:research" => true ; "task_allowed")]
    #[test_case("websearch", "rust async" => false ; "websearch_prompts")]
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
}
