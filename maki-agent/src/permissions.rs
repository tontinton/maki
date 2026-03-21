use std::cell::RefCell;
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

const BUILTIN_ALLOW_RULES: &[(&str, &str)] = &[
    ("write", "**"),
    ("edit", "**"),
    ("multiedit", "**"),
    ("code_execution", "*"),
    ("task", "*"),
    ("websearch", "*"),
    ("webfetch", "*"),
];

const COMPLEX_NODE_TYPES: &[&str] = &[
    "command_substitution",
    "process_substitution",
    "subshell",
    "arithmetic_expansion",
];

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
    NeedsPrompt { tool: String, scope: String },
}

#[derive(Debug, Error)]
pub enum PermissionError {
    #[error("Permission denied: {tool}")]
    Denied { tool: String },
    #[error("Permission denied (no response channel): {tool}")]
    NoResponseChannel { tool: String },
    #[error("Permission denied (channel closed): {tool}")]
    ChannelClosed { tool: String },
}

impl PermissionError {
    fn denied(tool: &str) -> Self {
        Self::Denied {
            tool: tool.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    AllowOnce,
    AllowSession,
    AllowAlwaysLocal,
    AllowAlwaysGlobal,
    DenyOnce,
    DenyAlwaysLocal,
    DenyAlwaysGlobal,
}

impl PermissionDecision {
    pub fn is_allow(self) -> bool {
        matches!(
            self,
            Self::AllowOnce | Self::AllowSession | Self::AllowAlwaysLocal | Self::AllowAlwaysGlobal
        )
    }

    pub fn to_answer_str(self) -> &'static str {
        match self {
            Self::AllowOnce => "allow",
            Self::AllowSession => "allow_session",
            Self::AllowAlwaysLocal => "allow_always_local",
            Self::AllowAlwaysGlobal => "allow_always_global",
            Self::DenyOnce => "deny",
            Self::DenyAlwaysLocal => "deny_always_local",
            Self::DenyAlwaysGlobal => "deny_always_global",
        }
    }

    pub fn from_answer_str(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(Self::AllowOnce),
            "allow_session" => Some(Self::AllowSession),
            "allow_always_local" => Some(Self::AllowAlwaysLocal),
            "allow_always_global" => Some(Self::AllowAlwaysGlobal),
            "deny" => Some(Self::DenyOnce),
            "deny_always_local" => Some(Self::DenyAlwaysLocal),
            "deny_always_global" => Some(Self::DenyAlwaysGlobal),
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
        let builtin_rules = BUILTIN_ALLOW_RULES
            .iter()
            .map(|(tool, scope)| PermissionRule {
                tool: tool.to_string(),
                scope: Some(scope.to_string()),
                effect: Effect::Allow,
            })
            .collect();
        Self {
            session_rules: Mutex::new(Vec::new()),
            config_rules: config.rules,
            builtin_rules,
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
            scope: pending.join(" && "),
        }
    }

    pub fn check_bash(&self, command: &str) -> PermissionCheck {
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

    pub fn apply_decision(&self, tool: &str, scope: &str, decision: PermissionDecision) {
        match decision {
            PermissionDecision::AllowOnce | PermissionDecision::DenyOnce => {}
            PermissionDecision::AllowSession => {
                self.add_session_rule(PermissionRule {
                    tool: tool.to_string(),
                    scope: Some(scope.to_string()),
                    effect: Effect::Allow,
                });
            }
            PermissionDecision::AllowAlwaysLocal
            | PermissionDecision::AllowAlwaysGlobal
            | PermissionDecision::DenyAlwaysLocal
            | PermissionDecision::DenyAlwaysGlobal => {
                let effect = if decision.is_allow() {
                    Effect::Allow
                } else {
                    Effect::Deny
                };
                let target = match decision {
                    PermissionDecision::AllowAlwaysLocal | PermissionDecision::DenyAlwaysLocal => {
                        PermissionTarget::Project(self.cwd.clone())
                    }
                    _ => PermissionTarget::Global,
                };
                let segments = if tool == "bash" {
                    split_shell_commands(scope)
                } else {
                    vec![]
                };
                let scopes: Vec<&str> = if segments.is_empty() {
                    vec![scope]
                } else {
                    segments.iter().map(|s| s.as_str()).collect()
                };
                for s in scopes {
                    let persisted = if effect == Effect::Allow {
                        generalize_scope(tool, s)
                    } else {
                        s.to_string()
                    };
                    self.add_session_rule(PermissionRule {
                        tool: tool.to_string(),
                        scope: Some(persisted.clone()),
                        effect,
                    });
                    if let Err(e) = append_permission_rule(tool, Some(&persisted), effect, &target)
                    {
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
            PermissionCheck::Denied => Err(PermissionError::denied(tool)),
            PermissionCheck::NeedsPrompt {
                tool: pt,
                scope: ps,
            } => {
                let Some(rx) = user_response_rx else {
                    return Err(PermissionError::NoResponseChannel {
                        tool: tool.to_string(),
                    });
                };
                let guard = rx.lock().await;
                match self.check(&pt, &ps) {
                    PermissionCheck::Allowed => {
                        drop(guard);
                        Ok(())
                    }
                    PermissionCheck::Denied => {
                        drop(guard);
                        Err(PermissionError::denied(tool))
                    }
                    PermissionCheck::NeedsPrompt {
                        tool: t2,
                        scope: s2,
                    } => {
                        let _ = event_tx.send(AgentEvent::PermissionRequest {
                            id: request_id.to_owned(),
                            tool: t2.clone(),
                            scope: s2.clone(),
                        });
                        let response = cancel.race(guard.recv_async()).await;
                        drop(guard);
                        let answer = match response {
                            Ok(Ok(a)) => a,
                            Ok(Err(_)) => {
                                return Err(PermissionError::ChannelClosed {
                                    tool: tool.to_string(),
                                });
                            }
                            Err(_) => return Err(PermissionError::denied(tool)),
                        };
                        match PermissionDecision::from_answer_str(&answer) {
                            Some(d) => {
                                self.apply_decision(&t2, &s2, d);
                                if d.is_allow() {
                                    Ok(())
                                } else {
                                    Err(PermissionError::denied(tool))
                                }
                            }
                            None => Err(PermissionError::denied(tool)),
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
    let root = tree.root_node();
    let is_complex = has_complex_node(root) || has_error_node(root);
    if is_complex {
        return (vec![command.to_string()], true);
    }
    let mut segments = Vec::new();
    collect_commands(root, command, &mut segments);
    if segments.is_empty() {
        (vec![command.to_string()], false)
    } else {
        (segments, false)
    }
}

pub fn has_complex_shell_constructs(command: &str) -> bool {
    let Some(tree) = parse_bash(command) else {
        return true;
    };
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

#[cfg(test)]
fn bash_permission_scopes(command: &str) -> Vec<String> {
    if has_complex_shell_constructs(command) {
        return vec![command.to_string()];
    }
    let segments = split_shell_commands(command);
    if segments.is_empty() {
        vec![command.to_string()]
    } else {
        segments
    }
}

pub fn canonicalize_scope_path(path: &str) -> String {
    let p = Path::new(path);
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
            if result.is_relative()
                && let Ok(cwd) = std::env::current_dir()
            {
                return cwd.join(result).to_string_lossy().into_owned();
            }
            result.to_string_lossy().into_owned()
        }
    }
}

fn generalize_bash_segment(segment: &str) -> String {
    let first_token = segment.split_whitespace().next().unwrap_or(segment);
    format!("{first_token} *")
}

pub fn generalize_scope(tool: &str, scope: &str) -> String {
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

    #[test_case("cargo test" => vec!["cargo test"] ; "single_command")]
    #[test_case("cd /tmp && cargo test" => vec!["cd /tmp", "cargo test"] ; "two_commands_and")]
    #[test_case("a && b && c" => vec!["a", "b", "c"] ; "three_commands_and")]
    #[test_case("a || b" => vec!["a", "b"] ; "two_commands_or")]
    #[test_case("a && b || c" => vec!["a", "b", "c"] ; "mixed_and_or")]
    #[test_case("a ; b" => vec!["a", "b"] ; "semicolon")]
    #[test_case("a ; b && c" => vec!["a", "b", "c"] ; "semicolon_and_and")]
    #[test_case("cargo test 2>&1 | tail -5" => vec!["cargo test 2>&1", "tail -5"] ; "pipe_splits_into_segments")]
    #[test_case("echo \"a && b\"" => vec!["echo \"a && b\""] ; "double_quoted_and")]
    #[test_case("echo 'a && b'" => vec!["echo 'a && b'"] ; "single_quoted_and")]
    #[test_case("echo a \\&\\& b" => vec!["echo a \\&\\& b"] ; "escaped_ampersands")]
    #[test_case("echo \"a || b\" && cargo test" => vec!["echo \"a || b\"", "cargo test"] ; "quoted_or_with_real_and")]
    #[test_case("  cd /tmp  &&  cargo test  " => vec!["cd /tmp", "cargo test"] ; "extra_whitespace")]
    #[test_case("" => Vec::<String>::new() ; "empty_string")]
    #[test_case("   " => Vec::<String>::new() ; "only_whitespace")]
    #[test_case("echo 'it'\\''s'" => vec!["echo 'it'\\''s'"] ; "escaped_single_quote_in_single_quotes")]
    #[test_case("a | b" => vec!["a", "b"] ; "pipe_splits")]
    #[test_case("a && b | c" => vec!["a", "b", "c"] ; "and_then_pipe")]
    #[test_case("a | b && c | d" => vec!["a", "b", "c", "d"] ; "nested_pipe_in_list")]
    #[test_case("echo \"hello \\\"world\\\"\"" => vec!["echo \"hello \\\"world\\\"\""] ; "escaped_double_quote_in_double_quotes")]
    #[test_case("a&&b" => vec!["a", "b"] ; "no_spaces_around_and")]
    #[test_case("a||b" => vec!["a", "b"] ; "no_spaces_around_or")]
    fn test_split_shell_commands(input: &str) -> Vec<String> {
        split_shell_commands(input)
    }

    #[test_case("*", "anything" => true ; "star_matches_all")]
    #[test_case("**", "anything" => true ; "double_star_matches_all")]
    #[test_case("cargo *", "cargo test" => true ; "prefix_star")]
    #[test_case("cargo *", "git push" => false ; "prefix_no_match")]
    #[test_case("src/**", "src/main.rs" => true ; "dir_glob")]
    #[test_case("src/**", "tests/main.rs" => false ; "dir_glob_no_match")]
    #[test_case("exact", "exact" => true ; "exact_match")]
    #[test_case("exact", "other" => false ; "exact_no_match")]
    fn test_scope_matches(pattern: &str, value: &str) -> bool {
        scope_matches(pattern, value)
    }

    #[test_case("src/**", "srcfoo" => false ; "dir_glob_no_bare_prefix")]
    #[test_case("src/**", "src" => true ; "dir_glob_matches_dir_itself")]
    fn test_scope_matches_extra(pattern: &str, value: &str) -> bool {
        scope_matches(pattern, value)
    }

    #[test]
    fn canonicalize_resolves_dot_segments() {
        let result = canonicalize_scope_path("/a/b/../c");
        assert_eq!(result, "/a/c");
    }

    #[test]
    fn allow_all_permits_complex_bash() {
        let mgr = PermissionManager::new(
            PermissionsConfig {
                allow_all: true,
                rules: vec![],
            },
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "echo $(date)"),
            PermissionCheck::Allowed
        ));
    }

    #[test_case("cargo test" => false ; "simple_command")]
    #[test_case("echo $(whoami)" => true ; "dollar_paren")]
    #[test_case("echo `whoami`" => true ; "backtick")]
    #[test_case("cat <(ls)" => true ; "process_substitution_in")]
    #[test_case("diff <(ls a) >(ls b)" => true ; "process_substitution_both")]
    #[test_case("echo $((1+2))" => true ; "arithmetic")]
    #[test_case("(cd /tmp && rm -rf *)" => true ; "subshell")]
    #[test_case("echo 'safe $(not expanded)'" => false ; "single_quoted_dollar_paren")]
    #[test_case("echo \"safe $(expanded)\"" => true ; "double_quoted_dollar_paren")]
    #[test_case("echo \\$(not expanded)" => true ; "escaped_dollar_paren_conservative")]
    #[test_case("cargo test && echo done" => false ; "compound_but_not_complex")]
    fn test_has_complex_shell_constructs(input: &str) -> bool {
        has_complex_shell_constructs(input)
    }

    #[test]
    fn complex_command_returns_whole_command() {
        let scopes = bash_permission_scopes("echo $(rm -rf /)");
        assert_eq!(scopes, vec!["echo $(rm -rf /)"]);
    }

    #[test]
    fn simple_compound_splits() {
        let scopes = bash_permission_scopes("cd /tmp && cargo test");
        assert_eq!(scopes, vec!["cd /tmp", "cargo test"]);
    }

    #[test]
    fn analyze_bash_simple_returns_segments_not_complex() {
        let (scopes, is_complex) = analyze_bash("cd /tmp && cargo test");
        assert_eq!(scopes, vec!["cd /tmp", "cargo test"]);
        assert!(!is_complex);
    }

    #[test]
    fn analyze_bash_complex_returns_whole_command_and_complex() {
        let (scopes, is_complex) = analyze_bash("echo $(rm -rf /)");
        assert_eq!(scopes, vec!["echo $(rm -rf /)"]);
        assert!(is_complex);
    }

    #[test]
    fn compound_command_allowed_when_all_segments_match() {
        let mgr = PermissionManager::new(
            make_config(vec![allow_rule("cd *"), allow_rule("cargo *")]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "cd /tmp && cargo test"),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn compound_command_needs_prompt_if_any_segment_unmatched() {
        let mgr = PermissionManager::new(
            make_config(vec![allow_rule("cargo *")]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "cd /tmp && cargo test"),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn compound_command_denied_if_any_segment_denied() {
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
    fn compound_deny_segment_overrides_allow_all() {
        let mgr = PermissionManager::new(
            PermissionsConfig {
                allow_all: true,
                rules: vec![deny_rule("rm *")],
            },
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "cd /tmp && rm -rf /"),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn pipe_requires_both_segments_allowed() {
        let mgr = PermissionManager::new(
            make_config(vec![allow_rule("cargo *"), allow_rule("tail *")]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "cargo test 2>&1 | tail -5"),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn pipe_denied_when_rhs_not_allowed() {
        let mgr = PermissionManager::new(
            make_config(vec![allow_rule("cargo *")]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "cargo test | rm -rf /"),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn pipe_denied_when_rhs_explicitly_denied() {
        let mgr = PermissionManager::new(
            make_config(vec![allow_rule("cargo *"), deny_rule("rm *")]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "cargo test | rm -rf /"),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn compound_command_with_semicolon() {
        let mgr = PermissionManager::new(
            make_config(vec![allow_rule("cd *"), allow_rule("cargo *")]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "cd /tmp ; cargo test"),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn compound_command_quoted_operator_not_split() {
        let mgr = PermissionManager::new(
            make_config(vec![allow_rule("echo *")]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "echo \"a && b\""),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn compound_command_three_segments_all_allowed() {
        let mgr = PermissionManager::new(
            make_config(vec![
                allow_rule("cd *"),
                allow_rule("cargo *"),
                allow_rule("echo *"),
            ]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "cd /tmp && cargo test && echo done"),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn compound_command_three_segments_middle_denied() {
        let mgr = PermissionManager::new(
            make_config(vec![
                allow_rule("cd *"),
                allow_rule("echo *"),
                deny_rule("rm *"),
            ]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "cd /tmp && rm -rf / && echo done"),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn command_substitution_always_prompts() {
        let mgr = PermissionManager::new(
            make_config(vec![allow_rule("echo *")]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "echo $(rm -rf /)"),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn subshell_always_prompts() {
        let mgr = PermissionManager::new(
            make_config(vec![allow_rule("cd *"), allow_rule("rm *")]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "(cd /tmp && rm -rf /)"),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn backtick_always_prompts() {
        let mgr = PermissionManager::new(
            make_config(vec![allow_rule("echo *")]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "echo `whoami`"),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn always_allow_compound_adds_generalized_rules() {
        let mgr = PermissionManager::new(PermissionsConfig::default(), PathBuf::from("/tmp"));
        mgr.apply_decision(
            "bash",
            "cd /tmp && cargo test --all",
            PermissionDecision::AllowAlwaysLocal,
        );
        assert!(matches!(
            mgr.check("bash", "cd /other && cargo build"),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn deny_always_compound_uses_exact_scopes() {
        let mgr = PermissionManager::new(PermissionsConfig::default(), PathBuf::from("/tmp"));
        mgr.apply_decision(
            "bash",
            "cd /tmp && cargo test",
            PermissionDecision::DenyAlwaysLocal,
        );
        assert!(matches!(
            mgr.check("bash", "cd /tmp"),
            PermissionCheck::Denied
        ));
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
    fn allow_all_permits_everything() {
        let mgr = PermissionManager::new(
            PermissionsConfig {
                allow_all: true,
                rules: vec![],
            },
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "rm -rf /"),
            PermissionCheck::Allowed
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

    #[test]
    fn builtin_allows_write() {
        let mgr = PermissionManager::new(PermissionsConfig::default(), PathBuf::from("/tmp"));
        assert!(matches!(
            mgr.check("write", "/some/path"),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn bash_needs_prompt_by_default() {
        let mgr = PermissionManager::new(PermissionsConfig::default(), PathBuf::from("/tmp"));
        assert!(matches!(
            mgr.check("bash", "cargo test"),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn config_allow_rule_matches() {
        let mgr = PermissionManager::new(
            make_config(vec![allow_rule("cargo *")]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "cargo test"),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check("bash", "rm -rf"),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn session_grant_works() {
        let mgr = PermissionManager::new(PermissionsConfig::default(), PathBuf::from("/tmp"));
        assert!(matches!(
            mgr.check("bash", "cargo test"),
            PermissionCheck::NeedsPrompt { .. }
        ));
        mgr.add_session_rule(allow_rule("cargo *"));
        assert!(matches!(
            mgr.check("bash", "cargo test"),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn session_deny_overrides_config_allow() {
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

    #[test_case("bash", "cargo test --all" => "cargo *" ; "bash_generalizes")]
    #[test_case("write", "/home/user/src/main.rs" => "/home/user/src/**" ; "write_generalizes")]
    #[test_case("edit", "main.rs" => "**" ; "edit_no_parent")]
    #[test_case("webfetch", "https://example.com" => "*" ; "webfetch_generalizes")]
    #[test_case("websearch", "rust async" => "*" ; "websearch_generalizes")]
    #[test_case("task", "task:research" => "task:research" ; "task_passthrough")]
    fn test_generalize_scope(tool: &str, scope: &str) -> String {
        generalize_scope(tool, scope)
    }

    #[test]
    fn apply_decision_session_persist() {
        let mgr = PermissionManager::new(PermissionsConfig::default(), PathBuf::from("/tmp"));
        mgr.apply_decision("bash", "cargo test", PermissionDecision::AllowSession);
        assert!(matches!(
            mgr.check("bash", "cargo test"),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn always_adds_generalized_session_rule() {
        let mgr = PermissionManager::new(PermissionsConfig::default(), PathBuf::from("/tmp"));
        mgr.apply_decision(
            "bash",
            "cargo test --all",
            PermissionDecision::AllowAlwaysLocal,
        );
        assert!(matches!(
            mgr.check("bash", "cargo test --all"),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check("bash", "cargo build"),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn deny_always_uses_exact_scope() {
        let mgr = PermissionManager::new(PermissionsConfig::default(), PathBuf::from("/tmp"));
        mgr.apply_decision(
            "bash",
            "cargo test --all",
            PermissionDecision::DenyAlwaysLocal,
        );
        assert!(matches!(
            mgr.check("bash", "cargo test --all"),
            PermissionCheck::Denied
        ));
        assert!(matches!(
            mgr.check("bash", "cargo build"),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn session_persist_uses_exact_scope() {
        let mgr = PermissionManager::new(PermissionsConfig::default(), PathBuf::from("/tmp"));
        mgr.apply_decision("bash", "cargo test --all", PermissionDecision::AllowSession);
        assert!(matches!(
            mgr.check("bash", "cargo test --all"),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check("bash", "cargo build"),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn wildcard_tool_matches_any() {
        let mgr = PermissionManager::new(
            make_config(vec![PermissionRule {
                tool: "*".into(),
                scope: None,
                effect: Effect::Deny,
            }]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "anything"),
            PermissionCheck::Denied
        ));
        assert!(matches!(
            mgr.check("write", "/any/path"),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn permission_decision_roundtrip() {
        let decisions = [
            PermissionDecision::AllowOnce,
            PermissionDecision::AllowSession,
            PermissionDecision::AllowAlwaysLocal,
            PermissionDecision::AllowAlwaysGlobal,
            PermissionDecision::DenyOnce,
            PermissionDecision::DenyAlwaysLocal,
            PermissionDecision::DenyAlwaysGlobal,
        ];
        for d in decisions {
            let s = d.to_answer_str();
            let parsed = PermissionDecision::from_answer_str(s).unwrap();
            assert_eq!(parsed, d);
        }
    }

    #[test]
    fn deny_once_does_not_add_rules() {
        let mgr = PermissionManager::new(PermissionsConfig::default(), PathBuf::from("/tmp"));
        mgr.apply_decision("bash", "cargo test", PermissionDecision::DenyOnce);
        assert!(matches!(
            mgr.check("bash", "cargo test"),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn allow_once_does_not_add_rules() {
        let mgr = PermissionManager::new(PermissionsConfig::default(), PathBuf::from("/tmp"));
        mgr.apply_decision("bash", "cargo test", PermissionDecision::AllowOnce);
        assert!(matches!(
            mgr.check("bash", "cargo test"),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn parse_error_treated_as_complex() {
        assert!(has_complex_shell_constructs("echo $((("));
    }

    #[test_case("echo 'safe $(not expanded)'" => false ; "single_quoted_not_complex")]
    #[test_case("echo \"unsafe $(expanded)\"" => true ; "double_quoted_is_complex")]
    fn test_quoting_context_awareness(input: &str) -> bool {
        has_complex_shell_constructs(input)
    }

    #[test]
    fn toggle_yolo() {
        let pm = PermissionManager::new(make_config(vec![]), PathBuf::from("/tmp"));
        assert!(!pm.is_yolo());
        assert!(pm.toggle_yolo());
        assert!(pm.is_yolo());
        assert!(!pm.toggle_yolo());
        assert!(!pm.is_yolo());
    }
}
