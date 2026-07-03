use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use maki_config::{
    DefaultEffect, Effect, FILE_WRITE_TOOLS, PermissionRule, PermissionTarget, PermissionsConfig,
    append_permission_rule,
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
        maki_storage::paths::canonicalize_clean(cwd).display()
    );
    let allow = |tool: &str, scope: &str| PermissionRule {
        tool: tool.into(),
        scope: Some(scope.into()),
        effect: Effect::Allow,
    };
    let mut rules: Vec<PermissionRule> = FILE_WRITE_TOOLS
        .iter()
        .map(|tool| allow(tool, &cwd_glob))
        .collect();
    rules.push(allow("task", "*"));
    rules
}

pub const BOUNDARY_UNVERIFIABLE_PREFIX: &str = "Cannot verify project boundary for";

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
    yolo: AtomicBool,
    default: DefaultEffect,
    tool_defaults: HashMap<String, DefaultEffect>,
    cwd: PathBuf,
}

impl PermissionManager {
    pub fn new(config: PermissionsConfig, cwd: PathBuf) -> Self {
        Self {
            builtin_rules: builtin_rules(&cwd),
            session_rules: Mutex::new(Vec::new()),
            config_rules: config.rules,
            yolo: AtomicBool::new(config.yolo),
            default: config.default,
            tool_defaults: config.tool_defaults,
            cwd,
        }
    }

    fn session_rules(&self) -> std::sync::MutexGuard<'_, Vec<PermissionRule>> {
        self.session_rules.lock().unwrap_or_else(|e| {
            warn!("permission mutex was poisoned, recovering");
            e.into_inner()
        })
    }

    fn check_inner(
        &self,
        tool: &str,
        scopes: &[&str],
        force_prompt: bool,
        plan_path: Option<&Path>,
    ) -> PermissionCheck {
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

        if self.yolo.load(Ordering::Relaxed) {
            return PermissionCheck::Allowed;
        }

        if FILE_WRITE_TOOLS.contains(&tool)
            && let Some(pp) = plan_path
        {
            let plan_scope = normalize_scope_path(&pp.display().to_string());
            if scopes.iter().any(|s| normalize_scope_path(s) == plan_scope) {
                return PermissionCheck::Allowed;
            }
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

        let eff = self
            .tool_defaults
            .get(tool)
            .copied()
            .unwrap_or(self.default);
        match eff {
            DefaultEffect::Deny => {
                info!(tool, "denied by default");
                PermissionCheck::Denied
            }
            DefaultEffect::Allow => PermissionCheck::Allowed,
            DefaultEffect::Prompt => PermissionCheck::NeedsPrompt {
                tool: tool.to_string(),
                scopes: pending.into_iter().map(|s| s.to_string()).collect(),
                force_prompt,
            },
        }
    }

    pub fn check(&self, tool: &str, scope: &str, plan_path: Option<&Path>) -> PermissionCheck {
        self.check_inner(tool, &[scope], false, plan_path)
    }

    pub fn check_multi(
        &self,
        tool: &str,
        scopes: &[&str],
        force_prompt: bool,
        plan_path: Option<&Path>,
    ) -> PermissionCheck {
        self.check_inner(tool, scopes, force_prompt, plan_path)
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
        let prev = self.yolo.fetch_xor(true, Ordering::Relaxed);
        !prev
    }

    pub fn is_yolo(&self) -> bool {
        self.yolo.load(Ordering::Relaxed)
    }

    /// Outside-cwd paths are not blocked here. They flow through the normal
    /// permission prompt (which uses the same canonicalization via
    /// [`scope_matches`]). Only unresolvable boundaries are hard-blocked.
    pub fn boundary_block_reason(&self, path: &Path) -> Option<String> {
        match physical_boundary_check(&self.cwd, path) {
            Some(_) => None,
            None => Some(format!(
                "{BOUNDARY_UNVERIFIABLE_PREFIX} {} \
                 (project root could not be resolved)",
                path.display()
            )),
        }
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

    #[allow(clippy::too_many_arguments)]
    pub async fn enforce(
        &self,
        tool: &str,
        scopes: &crate::tools::PermissionScopes,
        event_tx: &EventSender,
        user_response_rx: Option<&async_lock::Mutex<flume::Receiver<String>>>,
        request_id: &str,
        cancel: &crate::CancelToken,
        plan_path: Option<&Path>,
    ) -> Result<(), PermissionError> {
        let scope_refs: Vec<&str> = scopes.scopes.iter().map(|s| s.as_str()).collect();
        let deny = |guidance: Option<String>| {
            let display = scopes.scopes.join("; ");
            match guidance {
                Some(g) => PermissionError::with_guidance(tool, &display, g),
                None => PermissionError::new(tool, &display),
            }
        };

        let (pt, ps, force_prompt) =
            match self.check_inner(tool, &scope_refs, scopes.force_prompt, plan_path) {
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
        let (t2, s2) = match self.check_inner(&pt, &refs, force_prompt, plan_path) {
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

/// Glob matcher for permission scopes. The boundary suffixes (`/**`, `" *"`)
/// must be tried before the bare `*`, otherwise a plain prefix would swallow
/// them. `" *"` is the bash form `<command> *`: it has to match the bare
/// command too (`pwd *` covers `pwd` and `pwd -L`, but not `pwdx`).
///
/// For the `/**` path pattern, `Path::starts_with` is used to compare
/// components rather than characters, which handles both `/` and `\`
/// transparently on all platforms.
pub fn scope_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" || pattern == "**" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        let norm_prefix = maki_storage::paths::canonicalize_clean(Path::new(prefix));
        // Use incremental canonicalization for the value so symlinks in
        // existing path components are resolved before any `..` traversal.
        // `canonicalize_clean` falls back to lexical normalization when the
        // file doesn't exist, which breaks scope matching when the project
        // dir itself is a symlink.
        let norm_value = maki_storage::paths::incremental_canonicalize(Path::new(value))
            .unwrap_or_else(|| maki_storage::paths::normalize_path(Path::new(value)));
        return norm_value == norm_prefix || norm_value.starts_with(&norm_prefix);
    }
    if let Some(prefix) = pattern.strip_suffix(" *") {
        return value == prefix || value.starts_with(&format!("{prefix} "));
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return value.starts_with(prefix);
    }
    pattern == value
}

/// Lexical normalization for scope paths. Resolves `..` and `.` without
/// hitting the filesystem and without producing `\\?\` prefixes on Windows.
/// Use this for display, logging, and scope matching.
///
/// For symlink-aware security checks, use [`physical_boundary_check`].
pub fn normalize_scope_path(path: &str) -> String {
    let resolved = crate::tools::resolve_path(path).unwrap_or_else(|_| path.to_string());
    maki_storage::paths::normalize_path(Path::new(&resolved))
        .to_string_lossy()
        .into_owned()
}

/// Check whether `child` is physically inside `parent`, following symlinks.
///
/// Uses incremental left-to-right canonicalization: each component is
/// resolved through the filesystem (including symlinks) *before* any
/// subsequent `..` component can act on it. This prevents symlink-based
/// boundary escapes where a symlink followed by `..` resolves to a
/// location outside the parent.
///
/// Returns `true` only when the resolved filesystem location of `child`
/// is under `parent`. Returns `None` if the parent itself cannot be resolved.
pub fn physical_boundary_check(parent: &Path, child: &Path) -> Option<bool> {
    let parent_canon = maki_storage::paths::incremental_canonicalize(parent)?;
    let child_canon =
        maki_storage::paths::incremental_canonicalize(child).unwrap_or_else(|| child.to_path_buf());
    Some(child_canon.starts_with(&parent_canon))
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
        tool if FILE_WRITE_TOOLS.contains(&tool) => {
            let p = Path::new(scope);
            match p.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => {
                    format!("{}/**", parent.display())
                }
                _ => "**".to_string(),
            }
        }
        // MCP tool calls are dispatched as `mcp:<tool_name>` with a scope equal
        // to the JSON-stringified input. "Allow always" should whitelist the
        // tool regardless of its arguments, so generalize the scope to `*`. The
        // rule's `tool` field (`mcp:<tool_name>`) still gates which MCP tool it
        // applies to, keeping distinct tools distinct.
        t if t.starts_with("mcp:") => "*".to_string(),
        _ => scope.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn make_config(rules: Vec<PermissionRule>) -> PermissionsConfig {
        PermissionsConfig {
            rules,
            ..Default::default()
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
    #[test_case("pwd *", "pwd" => true ; "space_star_matches_bare_command")]
    #[test_case("pwd *", "pwd -L" => true ; "space_star_matches_with_args")]
    #[test_case("pwd *", "pwdx" => false ; "space_star_no_partial_token")]
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
        let check = mgr.check_multi("bash", &scopes, false, None);
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
            mgr.check_multi("bash", &["cd /tmp", "cargo test", "rm -rf /"], false, None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn complex_constructs_force_prompt_even_with_allow_star() {
        let mgr = PermissionManager::new(make_config(vec![allow_rule("*")]), PathBuf::from("/tmp"));
        assert!(matches!(
            mgr.check_multi("bash", &["echo $(whoami)"], true, None),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test_case("write", "/tmp/file.txt" => true ; "write_in_cwd")]
    #[test_case("write", "/etc/passwd" => false ; "write_outside_cwd")]
    #[test_case("task", "task:research" => true ; "task_allowed")]
    #[test_case("bash", "cargo test" => false ; "bash_prompts")]
    fn builtin_check(tool: &str, scope: &str) -> bool {
        matches!(
            default_mgr().check(tool, scope, None),
            PermissionCheck::Allowed
        )
    }

    #[test]
    #[cfg(unix)]
    fn scope_matches_resolves_symlinked_parent() {
        let tmp = std::env::temp_dir();
        let real = tmp.join("__maki_test_scope_symlink_real");
        let link = tmp.join("__maki_test_scope_symlink_link");
        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let pattern = format!("{}/**", real.display());
        let value = format!("{}/new_file.txt", link.display());
        assert!(
            scope_matches(&pattern, &value),
            "symlinked parent should resolve: pattern={pattern}, value={value}"
        );

        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
    }

    #[test]
    fn path_traversal_prompts() {
        let path = normalize_scope_path("/tmp/../etc/passwd");
        assert!(matches!(
            default_mgr().check("write", &path, None),
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
            mgr.check("bash", "cargo test", None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn deny_overrides_default_allow() {
        let mgr = PermissionManager::new(
            PermissionsConfig {
                default: DefaultEffect::Allow,
                rules: vec![deny_rule("rm *")],
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "rm -rf /", None),
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
            mgr.check("bash", "cargo build", None),
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
            mgr.check("bash", "cargo test", None),
            PermissionCheck::Denied
        ));
        assert!(matches!(
            mgr.check("bash", "cargo build", None),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn boundary_inside_proceeds() {
        let tmp = std::env::temp_dir();
        let mgr = PermissionManager::new(PermissionsConfig::default(), tmp.clone());
        assert!(
            mgr.boundary_block_reason(&tmp.join("some_file.txt"))
                .is_none()
        );
    }

    #[test]
    fn boundary_outside_proceeds_via_prompt() {
        let tmp = std::env::temp_dir();
        let mgr = PermissionManager::new(PermissionsConfig::default(), tmp);
        #[cfg(unix)]
        let outside = Path::new("/etc/hosts");
        #[cfg(windows)]
        let outside = Path::new(r"C:\Windows\System32\drivers\etc\hosts");
        assert!(mgr.boundary_block_reason(outside).is_none());
    }

    #[test]
    fn boundary_dotdot_smuggling_proceeds_via_prompt() {
        let tmp = std::env::temp_dir();
        let sub = tmp.join("__maki_test_boundary");
        std::fs::create_dir_all(&sub).unwrap();
        #[cfg(unix)]
        let attack = sub
            .join("x")
            .join("..")
            .join("..")
            .join("..")
            .join("etc")
            .join("passwd");
        #[cfg(windows)]
        let attack = sub
            .join("x")
            .join("..")
            .join("..")
            .join("..")
            .join("Windows")
            .join("System32");
        let mgr = PermissionManager::new(PermissionsConfig::default(), sub.clone());
        assert!(
            mgr.boundary_block_reason(&attack).is_none(),
            "outside-cwd dotdot path should prompt, not hard-block: {}",
            attack.display()
        );
        let _ = std::fs::remove_dir_all(&sub);
    }

    #[test]
    #[cfg(unix)]
    fn boundary_symlink_escape_proceeds_via_prompt() {
        // Lexical normalization resolves this inside (/project/escape), but
        // incremental canonicalization follows the symlink first, so `..`
        // escapes outside. The permission prompt catches it, not this function.
        let tmp = std::env::temp_dir();
        let project = tmp.join("__maki_test_symlink_escape");
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(&project).unwrap();
        let link = project.join("link");
        let _ = std::os::unix::fs::symlink(&tmp, &link);

        let attack = link.join("..").join("escape_target");
        let mgr = PermissionManager::new(PermissionsConfig::default(), project.clone());
        assert!(
            mgr.boundary_block_reason(&attack).is_none(),
            "outside-boundary edits are gated by the prompt, not hard-blocked: {}",
            attack.display()
        );
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn boundary_nonexistent_cwd_proceeds_via_lexical_tail() {
        let missing = std::env::temp_dir().join("__maki_test_absent_cwd_xyz");
        let _ = std::fs::remove_dir_all(&missing);
        let mgr = PermissionManager::new(PermissionsConfig::default(), missing.clone());
        assert!(
            mgr.boundary_block_reason(&missing.join("file.txt"))
                .is_none()
        );
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
            mgr.check_multi("bash", &["cargo test", "git push"], false, None),
            PermissionCheck::Allowed
        ));
        match mgr.check_multi("bash", &["cargo test", "git push"], true, None) {
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
            mgr.check_multi("bash", &["rm -rf /"], true, None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn check_multi_partial_coverage_prompts_uncovered() {
        let mgr = PermissionManager::new(
            make_config(vec![allow_rule("cargo *")]),
            PathBuf::from("/tmp"),
        );
        match mgr.check_multi("bash", &["cargo test", "git push", "ls"], false, None) {
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
            mgr.check("bash", "cargo build", None),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check("bash", "git push", None),
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

    #[test_case("webfetch", "some:scope" => "some:scope" ; "unknown_tool_preserves_exact")]
    #[test_case("mcp:fetch", "{\"url\":\"https://a\"}" => "*" ; "mcp_tool_generalizes_to_wildcard")]
    fn generalize_single_scope(tool: &str, scope: &str) -> String {
        generalized_scopes(tool, &[scope.into()])
            .into_iter()
            .next()
            .unwrap()
    }

    #[test]
    fn generalize_edit_uses_parent_dir() {
        let result = generalize_scope("edit", "/home/user/project/src/main.rs");
        let expected = format!(
            "{}/**",
            Path::new("/home/user/project/src/main.rs")
                .parent()
                .unwrap()
                .display()
        );
        assert_eq!(result, expected);
    }

    #[test]
    fn generalize_edit_root_file() {
        let result = generalize_scope("edit", "/Cargo.toml");
        let expected = format!(
            "{}/**",
            Path::new("/Cargo.toml").parent().unwrap().display()
        );
        assert_eq!(result, expected);
    }

    /// "Allow always" stores a command's generalized scope as a rule, so the
    /// command must match the very rule it would create. When this broke, the
    /// bare `pwd` never matched its own `pwd *` rule and we reprompted forever.
    #[test_case("bash", "pwd" ; "bash_bare_command")]
    #[test_case("bash", "cargo test" ; "bash_command_with_args")]
    #[test_case("bash", "git status --short" ; "bash_command_with_flags")]
    #[test_case("edit", "/home/user/project/src/main.rs" ; "edit_path")]
    #[test_case("webfetch", "https://example.com" ; "unknown_tool_exact")]
    #[test_case("mcp:fetch", "{\"url\":\"https://a\"}" ; "mcp_tool_call")]
    fn command_matches_its_own_generalized_rule(tool: &str, scope: &str) {
        let rule = &generalized_scopes(tool, &[scope.into()])[0];
        assert!(
            scope_matches(rule, scope),
            "{scope:?} does not match its generalized rule {rule:?}"
        );
    }

    /// "Allow always" on an MCP tool generalizes the stored scope to `*`, so a
    /// later call with different arguments matches the persisted rule instead of
    /// reprompting, while a different MCP tool is still gated by its `tool` name.
    #[test]
    fn mcp_allow_always_matches_any_args_but_stays_per_tool() {
        let mgr = default_mgr();
        mgr.apply_decision(
            "mcp:fetch",
            &["{\"url\":\"https://a\"}".into()],
            &PermissionAnswer::AllowSession,
        );
        // Same tool, different arguments -> allowed without reprompting.
        assert!(matches!(
            mgr.check("mcp:fetch", "{\"url\":\"https://b\"}", None),
            PermissionCheck::Allowed
        ));
        // A distinct MCP tool is not covered by the fetch rule.
        assert!(!matches!(
            mgr.check("mcp:exec", "{\"cmd\":\"ls\"}", None),
            PermissionCheck::Allowed
        ));
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
            mgr.check("bash", "anything", None),
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
        assert!(matches!(
            mgr.check("bash", "ls", None),
            PermissionCheck::Denied
        ));
        assert!(matches!(
            mgr.check("write", "/tmp/x", None),
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
            mgr.check("bash", "cargo test", None),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check("bash", "rm -rf /", None),
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

    #[test]
    fn default_deny_blocks_unmatched() {
        let mgr = PermissionManager::new(
            PermissionsConfig {
                default: DefaultEffect::Deny,
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "cargo test", None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn default_deny_with_allow_rules() {
        let mgr = PermissionManager::new(
            PermissionsConfig {
                default: DefaultEffect::Deny,
                rules: vec![allow_rule("cargo *")],
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "cargo test", None),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check("bash", "rm -rf /", None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn default_allow_allows_unmatched() {
        let mgr = PermissionManager::new(
            PermissionsConfig {
                default: DefaultEffect::Allow,
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "cargo test", None),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn default_prompt_is_default_behavior() {
        let mgr = PermissionManager::new(PermissionsConfig::default(), PathBuf::from("/tmp"));
        assert!(matches!(
            mgr.check("bash", "cargo test", None),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn per_tool_default_overrides_global() {
        let mgr = PermissionManager::new(
            PermissionsConfig {
                default: DefaultEffect::Deny,
                tool_defaults: HashMap::from([("bash".into(), DefaultEffect::Allow)]),
                rules: vec![],
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check("bash", "cargo test", None),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check("write", "/etc/passwd", None),
            PermissionCheck::Denied
        ));
    }

    #[test_case("write", true ; "write_tool_allowed")]
    #[test_case("edit", true ; "edit_tool_allowed")]
    #[test_case("bash", false ; "non_write_tool_prompts")]
    fn plan_path_auto_allows_file_write_tools_only(tool: &str, expect_allowed: bool) {
        let plan = "/home/user/.local/state/maki/plans/test.md";
        let plan_path = Path::new(plan);
        let mgr = default_mgr();
        assert_eq!(
            matches!(
                mgr.check(tool, plan, Some(plan_path)),
                PermissionCheck::Allowed
            ),
            expect_allowed,
        );
    }
}
