use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use maki_config::{
    DefaultEffect, Effect, FILE_WRITE_TOOLS, PermissionRule, PermissionTarget, PermissionsConfig,
    ToolKey, append_permission_rule,
};
use thiserror::Error;
use tracing::{info, warn};

use maki_providers::Timeouts;
use serde_json::Value;

use crate::reviewers::{
    AttemptRecord, DEFAULT_MAX_REDIRECTS_PER_TURN, FINAL_REDIRECT_GUIDANCE, LinkCx, LinkOutcome,
    ProviderTransport, REDIRECT_GUIDANCE, ReviewCall, ReviewTransport, ReviewerDef, Verdict,
    build_user_message,
};
use crate::{AgentEvent, EventSender, ReviewerVerdictEvent};

/// Hard cap on registered reviewers naming any one tool pattern; a runaway
/// plugin loop cannot grow the chain past this and blow the walk budget.
pub const MAX_REVIEWER_CHAIN: usize = 8;

pub const DEFAULT_DENY_GUIDANCE: &str =
    "Do not retry. Try a different approach or ask the user for guidance.";

/// Tests assert on this exact prefix; a wording tweak here updates them in one place.
pub const PERMISSION_DENIED_PREFIX: &str = "Permission denied for";

/// Values for the `source` attribute on `maki.tool_decision` events.
pub const DECISION_SOURCE_RULE: &str = "rule";
pub const DECISION_SOURCE_YOLO: &str = "yolo";
pub const DECISION_SOURCE_REVIEWER: &str = "reviewer";
pub const DECISION_SOURCE_USER_ONCE: &str = "user_once";
pub const DECISION_SOURCE_USER_SESSION: &str = "user_session";
pub const DECISION_SOURCE_USER_ALWAYS: &str = "user_always";
pub const DECISION_SOURCE_USER_ABORT: &str = "user_abort";

const TASK_TOOL: &str = "task";
const BASH_TOOL: &str = "bash";

fn builtin_rules(cwd: &Path) -> Vec<PermissionRule> {
    let cwd_glob = format!(
        "{}/**",
        maki_storage::paths::canonicalize_clean(cwd).display()
    );
    let allow = |tool: &str, scope: &str| PermissionRule {
        tool: ToolKey::native(tool),
        scope: Some(scope.into()),
        effect: Effect::Allow,
    };
    let mut rules: Vec<PermissionRule> = FILE_WRITE_TOOLS
        .iter()
        .map(|tool| allow(tool, &cwd_glob))
        .collect();
    rules.push(allow(TASK_TOOL, "*"));
    rules
}

pub const BOUNDARY_UNVERIFIABLE_PREFIX: &str = "Cannot verify project boundary for";

/// Whether the builtin defaults treat `tool` specially. File write tools get
/// the cwd allow and the plan mode allow, `task` gets a blanket allow, and an
/// "allow always" for `bash` is stored under the first word of the command.
/// Rules are keyed by name alone, so a plugin taking one of these names
/// inherits all of it.
pub fn carries_builtin_defaults(tool: &str) -> bool {
    FILE_WRITE_TOOLS.contains(&tool) || matches!(tool, TASK_TOOL | BASH_TOOL)
}

#[derive(Debug)]
pub enum PermissionCheck {
    Allowed,
    Denied,
    NeedsPrompt {
        tool: ToolKey,
        scopes: Vec<String>,
        force_prompt: bool,
    },
}

#[derive(Clone, Copy)]
pub struct ReviewSource<'a> {
    pub input: Option<&'a Value>,
    /// Oldest first; the last is the most recent.
    pub recent_user_messages: &'a [Arc<str>],
    pub timeouts: Timeouts,
}

impl ReviewSource<'_> {
    pub fn none() -> Self {
        ReviewSource {
            input: None,
            recent_user_messages: &[],
            timeouts: Timeouts::default(),
        }
    }
}

enum ReviewDecision {
    Allow,
    Deny {
        reviewer: String,
        reason: Option<String>,
    },
    Undecided,
    /// Task cancellation preempted the chain; skip ledger and redirect
    /// bookkeeping so a cancel does not burn a retry slot or a redirect.
    Cancelled,
}

fn scope_hash(scopes: &[String]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    scopes.hash(&mut hasher);
    hasher.finish()
}

#[derive(Debug, Error)]
pub struct ReviewerChainOverflow {
    pub tool: String,
}

impl std::fmt::Display for ReviewerChainOverflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "reviewer chain for tool \"{}\" would exceed MAX_REVIEWER_CHAIN ({})",
            self.tool, MAX_REVIEWER_CHAIN
        )
    }
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

/// How squarely an approval names the tool being checked. Every source of
/// approval has to say which one it is, so the question cannot be skipped by a
/// source added later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Approval {
    /// A rule naming this tool. The user answered about this tool.
    ForThisTool,
    /// Yolo, a default, a `*` rule, a whole-server rule. It covers this tool
    /// along with others and says nothing about this one in particular.
    Standing,
}

/// Whether an approval counts for the tool at hand. An MCP tool is opaque, so
/// nothing here tells a repo search from a commit, and plan mode only stays
/// read-only while a standing yes never speaks for one. Native tools are known
/// quantities and keep every approval they had.
#[derive(Clone, Copy)]
struct ApprovalGate {
    opaque_under_plan_mode: bool,
}

impl ApprovalGate {
    fn new(tool: &ToolKey, plan_path: Option<&Path>) -> Self {
        Self {
            opaque_under_plan_mode: plan_path.is_some() && tool.is_mcp(),
        }
    }

    fn accepts(self, approval: Approval) -> bool {
        !self.opaque_under_plan_mode || approval == Approval::ForThisTool
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
    pub fn decision_source(&self) -> &'static str {
        match self {
            Self::AllowOnce | Self::Deny | Self::DenyWithGuidance(_) => DECISION_SOURCE_USER_ONCE,
            Self::AllowSession => DECISION_SOURCE_USER_SESSION,
            Self::AllowAlwaysLocal
            | Self::AllowAlwaysGlobal
            | Self::DenyAlwaysLocal
            | Self::DenyAlwaysGlobal => DECISION_SOURCE_USER_ALWAYS,
        }
    }

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

/// Permission rules declared by Lua plugins via
/// `maki.api.register_permission_rule`, keyed by plugin name. Shared between
/// the Lua runtime (writer, on plugin load/unload) and every
/// [`PermissionManager`] (reader).
#[derive(Default)]
pub struct PluginRuleStore {
    rules: Mutex<HashMap<Arc<str>, Vec<PermissionRule>>>,
    reviewers: Mutex<HashMap<Arc<str>, Vec<ReviewerDef>>>,
}

impl PluginRuleStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Arc<str>, Vec<PermissionRule>>> {
        self.rules.lock().unwrap_or_else(|e| {
            warn!("plugin rule mutex was poisoned, recovering");
            e.into_inner()
        })
    }

    fn lock_reviewers(&self) -> std::sync::MutexGuard<'_, HashMap<Arc<str>, Vec<ReviewerDef>>> {
        self.reviewers.lock().unwrap_or_else(|e| {
            warn!("reviewer mutex was poisoned, recovering");
            e.into_inner()
        })
    }

    /// An empty `rules` removes the entry, so a reload that registers
    /// nothing clears the stale rules.
    pub fn replace(&self, plugin: &str, rules: Vec<PermissionRule>) {
        let mut map = self.lock();
        if rules.is_empty() {
            map.remove(plugin);
        } else {
            map.insert(Arc::from(plugin), rules);
        }
    }

    /// Same-name registration replaces in place, so reloads never stack
    /// duplicates. Enforces [`MAX_REVIEWER_CHAIN`] per tool pattern to keep
    /// a runaway registrar from unbounded chain growth.
    pub fn add_reviewer(
        &self,
        plugin: &str,
        def: ReviewerDef,
    ) -> Result<(), ReviewerChainOverflow> {
        let mut map = self.lock_reviewers();
        let plugin_key: Arc<str> = Arc::from(plugin);
        let is_upsert = map
            .get(&plugin_key)
            .is_some_and(|defs| defs.iter().any(|existing| existing.name == def.name));
        if !is_upsert {
            for pat in &def.tools {
                let count = map
                    .values()
                    .flat_map(|defs| defs.iter())
                    .filter(|existing| existing.tools.iter().any(|p| p == pat))
                    .count();
                if count >= MAX_REVIEWER_CHAIN {
                    return Err(ReviewerChainOverflow { tool: pat.clone() });
                }
            }
        }
        let defs = map.entry(plugin_key).or_default();
        match defs.iter_mut().find(|existing| existing.name == def.name) {
            Some(slot) => *slot = def,
            None => defs.push(def),
        }
        Ok(())
    }

    /// Unknown names are a no-op so toggles can call this unconditionally.
    pub fn remove_reviewer(&self, plugin: &str, name: &str) {
        let mut map = self.lock_reviewers();
        let Some(defs) = map.get_mut(plugin) else {
            return;
        };
        defs.retain(|def| def.name.as_ref() != name);
        if defs.is_empty() {
            map.remove(plugin);
        }
    }

    pub fn replace_reviewers(&self, plugin: &str, defs: Vec<ReviewerDef>) {
        let mut map = self.lock_reviewers();
        if defs.is_empty() {
            map.remove(plugin);
        } else {
            map.insert(Arc::from(plugin), defs);
        }
    }

    pub fn remove(&self, plugin: &str) {
        self.lock().remove(plugin);
        self.lock_reviewers().remove(plugin);
    }

    pub fn snapshot(&self) -> Vec<PermissionRule> {
        self.lock().values().flatten().cloned().collect()
    }

    /// Matching reviewers ordered by `order`, plugin name, then registration
    /// order, so the walk is deterministic across plugins.
    pub fn reviewer_chain(&self, tool: &str) -> Vec<ReviewerDef> {
        self.reviewer_chain_where(|def| def.tools.iter().any(|pat| scope_matches(pat, tool)))
    }

    /// Only reviewers that named {tool} with a real pattern; the `"*"`
    /// default does not opt a reviewer into vetoing permission-free tools.
    pub fn explicit_reviewer_chain(&self, tool: &str) -> Vec<ReviewerDef> {
        self.reviewer_chain_where(|def| {
            def.tools
                .iter()
                .any(|pat| pat != "*" && scope_matches(pat, tool))
        })
    }

    fn reviewer_chain_where(&self, keep: impl Fn(&ReviewerDef) -> bool) -> Vec<ReviewerDef> {
        let map = self.lock_reviewers();
        let mut entries: Vec<(&Arc<str>, usize, &ReviewerDef)> = map
            .iter()
            .flat_map(|(plugin, defs)| {
                defs.iter()
                    .enumerate()
                    .map(move |(idx, def)| (plugin, idx, def))
            })
            .filter(|(_, _, def)| keep(def))
            .collect();
        entries.sort_by(|a, b| (a.2.order, a.0.as_ref(), a.1).cmp(&(b.2.order, b.0.as_ref(), b.1)));
        entries.into_iter().map(|(_, _, def)| def.clone()).collect()
    }

    pub fn has_reviewers(&self, tool: &str) -> bool {
        self.lock_reviewers()
            .values()
            .flatten()
            .any(|def| def.tools.iter().any(|pat| scope_matches(pat, tool)))
    }
}

pub struct PermissionManager {
    session_rules: Mutex<Vec<PermissionRule>>,
    config_rules: Vec<PermissionRule>,
    builtin_rules: Vec<PermissionRule>,
    yolo: AtomicBool,
    /// Whether the user set yolo for this session themselves, which is what
    /// makes it worth persisting.
    yolo_explicit: AtomicBool,
    /// What `--yolo` / `always_yolo` seeded `yolo` with, so a session with no
    /// stored intent falls back to the flag instead of to off.
    seed_yolo: bool,
    default: DefaultEffect,
    tool_defaults: HashMap<ToolKey, DefaultEffect>,
    cwd: PathBuf,
    plugin_rules: Arc<PluginRuleStore>,
    review_transport: Arc<dyn ReviewTransport>,
    review_ledger: Mutex<HashMap<(String, u64), AttemptRecord>>,
    turn_redirects: AtomicU32,
}

impl PermissionManager {
    pub fn new(
        config: PermissionsConfig,
        cwd: PathBuf,
        plugin_rules: Arc<PluginRuleStore>,
    ) -> Self {
        let config_rules = config.rules;
        let builtin_rules = builtin_rules(&cwd);

        // Warn if wildcard deny is present — it blocks ALL tools including builtins.
        let has_wildcard_deny = config_rules
            .iter()
            .any(|r| matches!(r.tool, ToolKey::Wildcard) && r.effect == Effect::Deny);
        if has_wildcard_deny {
            warn!(
                "wildcard deny detected — this blocks ALL tools including \
                 builtins (write/edit/multiedit/task). Use per-tool rules \
                 instead if you want selective access."
            );
        }
        // Warn if wildcard allow is present — it permits ALL tools including write/edit/task.
        let has_wildcard_allow = config_rules
            .iter()
            .any(|r| matches!(r.tool, ToolKey::Wildcard) && r.effect == Effect::Allow);
        if has_wildcard_allow {
            warn!(
                "wildcard allow detected — this permits ALL tools including \
                 write/edit/multiedit/task. Use per-tool rules \
                 instead if you want selective access."
            );
        }

        Self {
            builtin_rules,
            session_rules: Mutex::new(Vec::new()),
            config_rules,
            yolo: AtomicBool::new(config.yolo),
            yolo_explicit: AtomicBool::new(false),
            seed_yolo: config.yolo,
            default: config.default,
            tool_defaults: config.tool_defaults,
            cwd,
            plugin_rules,
            review_transport: Arc::new(ProviderTransport::default()),
            review_ledger: Mutex::new(HashMap::new()),
            turn_redirects: AtomicU32::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_transport(mut self, transport: Arc<dyn ReviewTransport>) -> Self {
        self.review_transport = transport;
        self
    }

    /// Fresh manager for a new session runtime: shares config and builtin
    /// rules plus the current yolo state, but owns empty session rules so
    /// restoring one session never clobbers another's grants.
    pub fn fork(&self) -> Self {
        Self {
            session_rules: Mutex::new(Vec::new()),
            config_rules: self.config_rules.clone(),
            builtin_rules: self.builtin_rules.clone(),
            yolo: AtomicBool::new(self.is_yolo()),
            yolo_explicit: AtomicBool::new(self.yolo_explicit.load(Ordering::Relaxed)),
            seed_yolo: self.seed_yolo,
            default: self.default,
            tool_defaults: self.tool_defaults.clone(),
            cwd: self.cwd.clone(),
            plugin_rules: Arc::clone(&self.plugin_rules),
            review_transport: Arc::clone(&self.review_transport),
            review_ledger: Mutex::new(HashMap::new()),
            turn_redirects: AtomicU32::new(0),
        }
    }

    fn session_rules(&self) -> std::sync::MutexGuard<'_, Vec<PermissionRule>> {
        self.session_rules.lock().unwrap_or_else(|e| {
            warn!("permission mutex was poisoned, recovering");
            e.into_inner()
        })
    }

    /// The order of the checks below is the policy itself, not an accident of
    /// how it was written: denies first, then yolo, then explicit allows, the
    /// plan file write, and last the defaults. Moving one moves the rules.
    /// Every approval among them goes through [`ApprovalGate`], which is what
    /// keeps plan mode's hold from depending on which one happens to run first.
    fn check_inner(
        &self,
        tool: &ToolKey,
        scopes: &[&str],
        force_prompt: bool,
        plan_path: Option<&Path>,
    ) -> PermissionCheck {
        let session = self.session_rules();
        let plugin = self.plugin_rules.snapshot();

        let gate = ApprovalGate::new(tool, plan_path);

        // Any matching deny wins, however broadly it was aimed. Only approvals
        // are ranked by how squarely they name the tool, and only plan mode
        // reads that rank.
        let mut unclaimed_scopes: Vec<&str> = if force_prompt {
            Vec::new()
        } else {
            Vec::with_capacity(scopes.len())
        };

        for scope in scopes {
            let mut has_allow = false;
            for r in session
                .iter()
                .chain(&self.config_rules)
                .chain(&self.builtin_rules)
                .chain(&plugin)
            {
                let Some(approval) = rule_reach(&r.tool, tool) else {
                    continue;
                };
                if !rule_matches_scope(r, scope) {
                    continue;
                }
                match r.effect {
                    Effect::Deny => {
                        info!(tool = %tool, scope = %scope, "permission denied");
                        return PermissionCheck::Denied;
                    }
                    Effect::Allow => has_allow |= gate.accepts(approval),
                }
            }

            if has_allow {
                // allow wins for this scope (no deny matched)
            } else if !force_prompt {
                unclaimed_scopes.push(scope);
            }
            // force_prompt: all scopes will be prompted anyway
        }

        // Yolo must not swallow the NeedsPrompt a registered reviewer intercepts.
        if self.yolo.load(Ordering::Relaxed)
            && gate.accepts(Approval::Standing)
            && !self.plugin_rules.has_reviewers(&tool.to_string())
        {
            return PermissionCheck::Allowed;
        }

        let pending: Vec<&str> = if force_prompt {
            scopes.to_vec()
        } else {
            unclaimed_scopes
        };

        if pending.is_empty() {
            return PermissionCheck::Allowed;
        }

        // Plan file auto-allow: fires AFTER deny rules have been evaluated.
        // Only triggers if ALL pending scopes match the plan file path.
        // A single non-plan scope means we must prompt for the rest.
        if !force_prompt && !pending.is_empty() {
            let is_plan_write = plan_path.is_some_and(|pp| {
                matches!(tool, ToolKey::Native(name) if FILE_WRITE_TOOLS.contains(&name.as_ref()))
                    && {
                        let normalized_plan = normalize_scope_path(&pp.display().to_string());
                        pending
                            .iter()
                            .all(|s| normalize_scope_path(s) == normalized_plan)
                    }
            });
            if is_plan_write {
                return PermissionCheck::Allowed;
            }
        }

        let eff = self
            .tool_defaults
            .get(tool)
            .copied()
            .or_else(|| {
                // McpTool falls back to McpServer-level default (Arc clone, ~2ns)
                let server = match tool {
                    ToolKey::McpTool { server, .. } => server,
                    _ => return None,
                };
                self.tool_defaults
                    .get(&ToolKey::McpServer {
                        server: server.clone(),
                    })
                    .copied()
            })
            .unwrap_or(self.default);
        match eff {
            DefaultEffect::Deny => {
                info!(tool = %tool, "denied by default");
                PermissionCheck::Denied
            }
            DefaultEffect::Allow if gate.accepts(Approval::Standing) => PermissionCheck::Allowed,
            DefaultEffect::Allow | DefaultEffect::Prompt => PermissionCheck::NeedsPrompt {
                tool: tool.clone(),
                scopes: pending.into_iter().map(|s| s.to_string()).collect(),
                force_prompt,
            },
        }
    }

    pub fn check(&self, tool: &ToolKey, scope: &str, plan_path: Option<&Path>) -> PermissionCheck {
        self.check_inner(tool, &[scope], false, plan_path)
    }

    pub fn check_multi(
        &self,
        tool: &ToolKey,
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

    /// The explicit toggle, so it also claims the session's intent: `/yolo` off
    /// under `--yolo` genuinely turns the session off and is remembered.
    pub fn toggle_yolo(&self) -> bool {
        let enabled = !self.yolo.fetch_xor(true, Ordering::Relaxed);
        self.yolo_explicit.store(true, Ordering::Relaxed);
        enabled
    }

    /// Replaces whatever this session was running with: `Some` is the user's
    /// stored intent, `None` means they never expressed one and the seed
    /// applies again.
    pub fn set_session_yolo(&self, stored: Option<bool>) {
        self.yolo
            .store(stored.unwrap_or(self.seed_yolo), Ordering::Relaxed);
        self.yolo_explicit
            .store(stored.is_some(), Ordering::Relaxed);
    }

    pub fn is_yolo(&self) -> bool {
        self.yolo.load(Ordering::Relaxed)
    }

    /// What the session may persist. A one-shot `--yolo` is a property of the
    /// invocation, so on its own it stores nothing.
    pub fn persisted_yolo(&self) -> Option<bool> {
        self.yolo_explicit
            .load(Ordering::Relaxed)
            .then(|| self.is_yolo())
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

    pub fn apply_decision(&self, tool: &ToolKey, scopes: &[String], answer: &PermissionAnswer) {
        let resolved = if answer.is_allow() || tool.is_mcp() {
            // MCP scopes are always wildcarded — both allow and deny generalize to "*".
            // This makes session and persisted rules consistent: a deny on an MCP tool
            // blocks the tool entirely, not just the specific input that triggered it.
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
                        tool: tool.clone(),
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
                        tool: tool.clone(),
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

    pub fn reset_review_turn(&self) {
        self.turn_redirects.store(0, Ordering::Relaxed);
        self.review_ledger().clear();
    }

    fn review_ledger(&self) -> std::sync::MutexGuard<'_, HashMap<(String, u64), AttemptRecord>> {
        self.review_ledger.lock().unwrap_or_else(|e| {
            warn!("review ledger mutex was poisoned, recovering");
            e.into_inner()
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_review_chain(
        &self,
        chain: &[ReviewerDef],
        tool: &ToolKey,
        scopes: &[String],
        force_prompt: bool,
        review: &ReviewSource<'_>,
        event_tx: &EventSender,
        cancel: &crate::CancelToken,
    ) -> ReviewDecision {
        let tool_string = tool.to_string();
        let ledger_key = (tool_string.clone(), scope_hash(scopes));
        let call = ReviewCall {
            tool: tool_string.clone(),
            input: review.input.cloned(),
            scopes: scopes.to_vec(),
            force_prompt,
            cwd: self.cwd.display().to_string(),
            recent_user_messages: review
                .recent_user_messages
                .iter()
                .map(|s| s.to_string())
                .collect(),
            attempt: self.review_ledger().get(&ledger_key).cloned(),
        };
        let user_message = build_user_message(&call);

        let mut decision = ReviewDecision::Undecided;
        for def in chain {
            if cancel.is_cancelled() {
                break;
            }
            let cx = LinkCx {
                transport: self.review_transport.as_ref(),
                timeouts: review.timeouts,
                user_message: &user_message,
            };
            let deadline = async {
                async_io::Timer::after(std::time::Duration::from_millis(def.timeout_ms)).await;
                LinkOutcome::default()
            };
            let raced = futures_lite::future::or(def.link.review(&call, cx), deadline);
            let outcome = cancel.race(raced).await.unwrap_or_default();
            let parsed = outcome.verdict;
            let (verdict, reason) = match &parsed {
                Some((verdict, reason)) => (verdict.as_str(), reason.clone()),
                None => ("ASK", None),
            };
            let resolution = match parsed {
                Some((Verdict::Allow, _)) => "allowed",
                Some((Verdict::Deny, _)) => "denied",
                _ => "escalated",
            };
            info!(
                tool = %tool_string,
                reviewer = %def.name,
                model = %def.link.label(),
                verdict,
                resolution,
                "reviewer verdict"
            );
            let _ = event_tx.send(AgentEvent::ReviewerVerdict(Box::new(
                ReviewerVerdictEvent {
                    tool: tool.clone(),
                    reviewer: def.name.to_string(),
                    model: def.link.label().to_owned(),
                    verdict: verdict.to_owned(),
                    reason: reason.clone(),
                    resolution: resolution.to_owned(),
                    usage: outcome.usage,
                    billed_cost: outcome.billed_cost,
                    list_cost: outcome.list_cost,
                },
            )));
            match parsed {
                Some((Verdict::Allow, _)) => {
                    decision = ReviewDecision::Allow;
                    break;
                }
                Some((Verdict::Deny, deny_reason)) => {
                    decision = ReviewDecision::Deny {
                        reviewer: def.name.to_string(),
                        reason: deny_reason,
                    };
                    break;
                }
                _ => {}
            }
        }

        if cancel.is_cancelled() {
            return ReviewDecision::Cancelled;
        }

        let mut ledger = self.review_ledger();
        let record = ledger.entry(ledger_key).or_insert_with(|| AttemptRecord {
            attempts: 0,
            history: Vec::new(),
        });
        record.attempts += 1;
        match &decision {
            ReviewDecision::Allow => record.record("ALLOW", None),
            ReviewDecision::Deny { reason, .. } => record.record("DENY", reason.as_deref()),
            ReviewDecision::Undecided => record.record("ASK", None),
            ReviewDecision::Cancelled => unreachable!("cancel returns early above"),
        }
        decision
    }

    /// Opt-in review for tools that need no permission: reviewers that
    /// named {tool} explicitly (not via the `"*"` default) get a chance to
    /// DENY the call. Anything short of a DENY — allow, timeout, exhausted
    /// escalation — falls through to normal execution, so an absent or slow
    /// reviewer can never break a free tool.
    pub async fn veto_review(
        &self,
        tool_name: &str,
        review: ReviewSource<'_>,
        event_tx: &EventSender,
        cancel: &crate::CancelToken,
    ) -> Result<(), PermissionError> {
        let chain = self.plugin_rules.explicit_reviewer_chain(tool_name);
        if chain.is_empty() {
            return Ok(());
        }
        let tool = ToolKey::native(tool_name);
        match self
            .run_review_chain(&chain, &tool, &[], false, &review, event_tx, cancel)
            .await
        {
            ReviewDecision::Deny { reviewer, reason } => {
                maki_otel::emit::tool_decision(
                    tool_name,
                    maki_otel::emit::DECISION_REJECT,
                    DECISION_SOURCE_REVIEWER,
                );
                let why = match reason {
                    Some(r) => format!("denied by reviewer {reviewer}: {r}"),
                    None => format!("denied by reviewer {reviewer}"),
                };
                Err(PermissionError::with_guidance(
                    tool_name,
                    "reviewer veto",
                    why,
                ))
            }
            ReviewDecision::Allow | ReviewDecision::Undecided | ReviewDecision::Cancelled => {
                maki_otel::emit::tool_decision(
                    tool_name,
                    maki_otel::emit::DECISION_ACCEPT,
                    DECISION_SOURCE_REVIEWER,
                );
                Ok(())
            }
        }
    }

    fn redirect_denial(
        &self,
        tool: &ToolKey,
        chain: &[ReviewerDef],
        event_tx: &EventSender,
    ) -> Option<String> {
        if !self.is_yolo() {
            return None;
        }
        let redirects = self.turn_redirects.fetch_add(1, Ordering::Relaxed) + 1;
        let guidance = if redirects >= DEFAULT_MAX_REDIRECTS_PER_TURN {
            FINAL_REDIRECT_GUIDANCE.to_owned()
        } else {
            chain
                .iter()
                .find_map(|def| def.redirect_guidance.clone())
                .unwrap_or_else(|| REDIRECT_GUIDANCE.to_owned())
        };
        let _ = event_tx.send(AgentEvent::ReviewerVerdict(Box::new(
            ReviewerVerdictEvent {
                tool: tool.clone(),
                reviewer: String::new(),
                model: String::new(),
                verdict: "ASK".to_owned(),
                reason: None,
                resolution: "redirected".to_owned(),
                usage: maki_providers::TokenUsage::default(),
                billed_cost: None,
                list_cost: None,
            },
        )));
        Some(guidance)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn enforce(
        &self,
        tool: &ToolKey,
        scopes: &crate::tools::PermissionScopes,
        event_tx: &EventSender,
        user_response_rx: Option<&async_lock::Mutex<flume::Receiver<String>>>,
        request_id: &str,
        cancel: &crate::CancelToken,
        plan_path: Option<&Path>,
        review: ReviewSource<'_>,
    ) -> Result<(), PermissionError> {
        let scope_refs: Vec<&str> = scopes.scopes.iter().map(|s| s.as_str()).collect();
        let tool_string = tool.to_string();
        let scope_display = || scopes.scopes.join("; ");
        // Every deny is built here and every approval passes through
        // `allowed`, so reporting cannot drift from what the caller gets.
        let deny = |source: &'static str, guidance: Option<String>| {
            maki_otel::emit::tool_decision(&tool_string, maki_otel::emit::DECISION_REJECT, source);
            match guidance {
                Some(g) => PermissionError::with_guidance(&tool_string, &scope_display(), g),
                None => PermissionError::new(&tool_string, &scope_display()),
            }
        };
        let allowed = |source: &'static str| {
            maki_otel::emit::tool_decision(&tool_string, maki_otel::emit::DECISION_ACCEPT, source);
            Ok(())
        };
        let by_rule = || {
            if self.yolo.load(Ordering::Relaxed) {
                DECISION_SOURCE_YOLO
            } else {
                DECISION_SOURCE_RULE
            }
        };

        let (pt, ps, force_prompt) =
            match self.check_inner(tool, &scope_refs, scopes.force_prompt, plan_path) {
                PermissionCheck::Allowed => return allowed(by_rule()),
                PermissionCheck::Denied => return Err(deny(DECISION_SOURCE_RULE, None)),
                PermissionCheck::NeedsPrompt {
                    tool,
                    scopes,
                    force_prompt,
                } => (tool, scopes, force_prompt),
            };

        let chain = self.plugin_rules.reviewer_chain(&tool_string);
        if !chain.is_empty() {
            match self
                .run_review_chain(&chain, tool, &ps, force_prompt, &review, event_tx, cancel)
                .await
            {
                ReviewDecision::Allow => return allowed(DECISION_SOURCE_REVIEWER),
                ReviewDecision::Deny { reviewer, reason } => {
                    let why = match reason {
                        Some(r) => format!("denied by reviewer {reviewer}: {r}"),
                        None => format!("denied by reviewer {reviewer}"),
                    };
                    return Err(deny(DECISION_SOURCE_REVIEWER, Some(why)));
                }
                ReviewDecision::Cancelled => return Err(deny(DECISION_SOURCE_USER_ABORT, None)),
                ReviewDecision::Undecided => {
                    if let Some(guidance) = self.redirect_denial(tool, &chain, event_tx) {
                        return Err(deny(DECISION_SOURCE_REVIEWER, Some(guidance)));
                    }
                    let _ = event_tx.send(AgentEvent::ReviewerVerdict(Box::new(
                        ReviewerVerdictEvent {
                            tool: tool.clone(),
                            reviewer: String::new(),
                            model: String::new(),
                            verdict: "ASK".to_owned(),
                            reason: None,
                            resolution: "prompted".to_owned(),
                            usage: maki_providers::TokenUsage::default(),
                            billed_cost: None,
                            list_cost: None,
                        },
                    )));
                }
            }
        }

        let Some(rx) = user_response_rx else {
            warn!(tool = %tool, scope = %scope_display(), "no permission response channel");
            return Err(deny(DECISION_SOURCE_USER_ABORT, None));
        };

        let guard = rx.lock().await;
        let refs: Vec<&str> = ps.iter().map(|s| s.as_str()).collect();
        let (t2, s2) = match self.check_inner(&pt, &refs, force_prompt, plan_path) {
            PermissionCheck::Allowed => return allowed(by_rule()),
            PermissionCheck::Denied => return Err(deny(DECISION_SOURCE_RULE, None)),
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
                warn!(tool = %tool, scope = %scope_display(), "permission channel closed");
                return Err(deny(DECISION_SOURCE_USER_ABORT, None));
            }
            Err(_) => return Err(deny(DECISION_SOURCE_USER_ABORT, None)),
        };

        let Some(answer) = PermissionAnswer::decode(&answer) else {
            return Err(deny(DECISION_SOURCE_USER_ABORT, None));
        };
        self.apply_decision(&t2, &s2, &answer);
        let source = answer.decision_source();
        if answer.is_allow() {
            allowed(source)
        } else {
            Err(deny(source, answer.guidance().map(String::from)))
        }
    }
}

/// Whether a rule reaches this tool, and how narrowly it was aimed if it does.
/// Both questions get answered by the one match, because a rule that reaches a
/// tool without saying how squarely is what let a `*` allow walk past plan mode.
fn rule_reach(rule_key: &ToolKey, actual: &ToolKey) -> Option<Approval> {
    match (rule_key, actual) {
        (ToolKey::Wildcard, _) => Some(Approval::Standing),
        (ToolKey::McpServer { server: rs }, ToolKey::McpTool { server: as_, .. }) => {
            (rs == as_).then_some(Approval::Standing)
        }
        (ToolKey::Native(a), ToolKey::Native(b)) => (a == b).then_some(Approval::ForThisTool),
        (ToolKey::McpServer { server: rs }, ToolKey::McpServer { server: as_ }) => {
            (rs == as_).then_some(Approval::ForThisTool)
        }
        (
            ToolKey::McpTool {
                server: rs,
                tool: rt,
            },
            ToolKey::McpTool {
                server: as_,
                tool: at,
            },
        ) => (rs == as_ && rt == at).then_some(Approval::ForThisTool),
        _ => None,
    }
}

fn rule_matches_scope(rule: &PermissionRule, scope: &str) -> bool {
    match &rule.scope {
        None => true,
        Some(pattern) => scope_matches(pattern, scope),
    }
}

/// A rule and the path it is matched against have to agree on what file they
/// name, including for a relative rule like `dist/**` written before the dir
/// exists — both are `canonical_key`'s job.
fn normalize_scope_prefix(path: &str) -> PathBuf {
    maki_storage::paths::canonical_key(Path::new(path))
}

/// A pattern with nothing left once its trailing glob is taken off covers
/// every scope: `*` and `**`, but also `/*` and `/**`, which reduce to a
/// prefix every absolute path starts with. [`scope_matches`] short-circuits on
/// these and a plugin allow is refused for them, off the same answer.
///
/// A `/**` prefix is normalized before the answer, the way the matcher reads
/// it, not compared as text. `//**`, `/./**` and `/tmp/../**` all name the
/// root once normalized, and going by their spelling would let a plugin
/// smuggle in the everything rule this refuses.
pub fn is_universal_scope(pattern: &str) -> bool {
    match pattern.strip_suffix("/**") {
        Some(prefix) => is_root(&normalize_scope_prefix(prefix)),
        None => {
            let stem = pattern.trim_end_matches('*');
            stem.len() < pattern.len() && matches!(stem, "" | "/")
        }
    }
}

fn is_root(path: &Path) -> bool {
    path.parent().is_none()
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
    if let Some(prefix) = pattern.strip_suffix("/**") {
        let norm_prefix = normalize_scope_prefix(prefix);
        // A root prefix covers every scope, bash commands included. Those are
        // not paths, so a plain prefix test would miss them.
        if is_root(&norm_prefix) {
            return true;
        }
        let norm_value = normalize_scope_prefix(value);
        return norm_value == norm_prefix || norm_value.starts_with(&norm_prefix);
    }
    if is_universal_scope(pattern) {
        return true;
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

pub fn generalized_scopes(tool: &ToolKey, scopes: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    scopes
        .iter()
        .map(|s| generalize_scope(tool, s))
        .filter(|g| seen.insert(g.clone()))
        .collect()
}

fn generalize_scope(tool: &ToolKey, scope: &str) -> String {
    match tool {
        ToolKey::Native(name) if name.as_ref() == BASH_TOOL => generalize_bash_segment(scope),
        ToolKey::Native(name) if FILE_WRITE_TOOLS.contains(&name.as_ref()) => {
            let p = Path::new(scope);
            match p.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => {
                    format!("{}/**", parent.display())
                }
                _ => "**".to_string(),
            }
        }
        // MCP tool calls have a scope equal to the JSON-stringified input.
        // "Allow always" should whitelist the tool regardless of its arguments,
        // so generalize the scope to `*`. The rule's `tool` field still gates
        // which MCP tool it applies to, keeping distinct tools distinct.
        ToolKey::McpTool { .. } | ToolKey::McpServer { .. } => "*".to_string(),
        _ => scope.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const PLAN_FILE: &str = "/home/user/.local/state/maki/plans/test.md";
    const TEST_CWD: &str = "/tmp";
    const MCP_SERVER: &str = "deepwiki";
    const MCP_TOOL: &str = "deepwiki.search";
    const MCP_ARGS: &str = "{\"q\":\"maki\"}";
    const READ_TOOL: &str = "read";
    const READ_SCOPE: &str = "/home/user/project/src/main.rs";

    const ALLOWED: &str = "allowed";
    const DENIED: &str = "denied";
    const PROMPTS: &str = "prompts";

    fn outcome(check: PermissionCheck) -> &'static str {
        match check {
            PermissionCheck::Allowed => ALLOWED,
            PermissionCheck::Denied => DENIED,
            PermissionCheck::NeedsPrompt { .. } => PROMPTS,
        }
    }

    fn mcp_tool_key() -> ToolKey {
        ToolKey::parse(MCP_TOOL).expect("test tool key parses")
    }

    fn native_key() -> ToolKey {
        ToolKey::Native(READ_TOOL.into())
    }

    fn mcp_server_key() -> ToolKey {
        ToolKey::McpServer {
            server: MCP_SERVER.into(),
        }
    }

    fn make_config(rules: Vec<PermissionRule>) -> PermissionsConfig {
        PermissionsConfig {
            rules,
            ..Default::default()
        }
    }

    mod veto {
        use super::*;
        use crate::reviewers::{LinkCx, LinkOutcome, ReviewCall, ReviewLink, parse_verdict};
        use maki_providers::provider::BoxFuture;

        struct FixedLink(&'static str);

        impl ReviewLink for FixedLink {
            fn label(&self) -> &str {
                "fixed"
            }

            fn review<'a>(
                &'a self,
                _call: &'a ReviewCall,
                _cx: LinkCx<'a>,
            ) -> BoxFuture<'a, LinkOutcome> {
                Box::pin(async move {
                    LinkOutcome {
                        verdict: parse_verdict(self.0),
                        ..Default::default()
                    }
                })
            }
        }

        fn manager_with(tools: Vec<&str>, verdict: &'static str) -> PermissionManager {
            let store = Arc::new(PluginRuleStore::default());
            store
                .add_reviewer(
                    "goal",
                    ReviewerDef {
                        name: Arc::from("goal-no-questions"),
                        link: Arc::new(FixedLink(verdict)),
                        tools: tools.into_iter().map(str::to_owned).collect(),
                        timeout_ms: 1_000,
                        order: 0,
                        redirect_guidance: None,
                    },
                )
                .expect("chain cap not reached in test");
            PermissionManager::new(make_config(Vec::new()), "/work".into(), store)
        }

        fn veto(mgr: &PermissionManager, tool: &str) -> Result<(), PermissionError> {
            let (tx, _rx) = flume::unbounded();
            let event_tx = EventSender::new(tx, 0);
            smol::block_on(mgr.veto_review(
                tool,
                ReviewSource::none(),
                &event_tx,
                &crate::CancelToken::none(),
            ))
        }

        #[test]
        fn explicit_deny_blocks_a_permission_free_tool() {
            let mgr = manager_with(vec!["question"], "DENY: goal mode is active");
            let err = veto(&mgr, "question")
                .expect_err("deny must block")
                .to_string();
            assert!(err.contains("goal-no-questions"), "err: {err}");
            assert!(err.contains("goal mode is active"), "err: {err}");
        }

        #[test]
        fn wildcard_reviewers_are_not_consulted() {
            let mgr = manager_with(vec!["*"], "DENY: would block everything");
            assert!(veto(&mgr, "question").is_ok());
        }

        #[test]
        fn anything_short_of_deny_lets_the_call_run() {
            for verdict in ["ALLOW", "ASK", "gibberish"] {
                let mgr = manager_with(vec!["question"], verdict);
                assert!(veto(&mgr, "question").is_ok(), "verdict {verdict}");
            }
        }

        #[test]
        fn glob_pattern_counts_as_explicit() {
            let mgr = manager_with(vec!["quest*"], "DENY");
            assert!(veto(&mgr, "question").is_err());
            assert!(veto(&mgr, "bash").is_ok(), "non-matching tool untouched");
        }
    }

    fn allow_rule(scope: &str) -> PermissionRule {
        PermissionRule {
            tool: ToolKey::native("bash"),
            scope: Some(scope.into()),
            effect: Effect::Allow,
        }
    }

    fn deny_rule(scope: &str) -> PermissionRule {
        PermissionRule {
            tool: ToolKey::native("bash"),
            scope: Some(scope.into()),
            effect: Effect::Deny,
        }
    }

    fn mgr_with(config: PermissionsConfig, cwd: PathBuf) -> PermissionManager {
        PermissionManager::new(config, cwd, Arc::default())
    }

    fn default_mgr() -> PermissionManager {
        mgr_with(PermissionsConfig::default(), PathBuf::from("/tmp"))
    }

    fn plugin_edit_rule(scope: &str, effect: Effect) -> PermissionRule {
        PermissionRule {
            tool: ToolKey::native("edit"),
            scope: Some(scope.into()),
            effect,
        }
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

    /// A pattern is universal only when the trailing glob is all it says.
    #[test_case("*" => true ; "star")]
    #[test_case("**" => true ; "double_star")]
    #[test_case("/*" => true ; "root_star")]
    #[test_case("/**" => true ; "root_double_star")]
    #[test_case("//**" => true ; "doubled_root_slash")]
    #[test_case("/./**" => true ; "root_dot")]
    #[test_case("/tmp/../**" => true ; "root_by_parent")]
    #[test_case("/tmp/**" => false ; "directory_subtree")]
    #[test_case("cargo *" => false ; "bash_command")]
    #[test_case("/" => false ; "root_without_glob")]
    #[test_case("" => false ; "empty")]
    fn universal_scope(pattern: &str) -> bool {
        is_universal_scope(pattern)
    }

    #[test_case(vec!["cd /tmp", "cargo test"], vec!["cd *", "cargo *"], true ; "all_allowed")]
    #[test_case(vec!["cd /tmp", "cargo test"], vec!["cargo *"], false ; "missing_rule")]
    fn compound_check(scopes: Vec<&str>, rules: Vec<&str>, expect_allowed: bool) {
        let mgr = mgr_with(
            make_config(rules.into_iter().map(allow_rule).collect()),
            PathBuf::from("/tmp"),
        );
        let check = mgr.check_multi(&ToolKey::native("bash"), &scopes, false, None);
        assert_eq!(matches!(check, PermissionCheck::Allowed), expect_allowed);
    }

    #[test]
    fn compound_denied_if_any_segment_denied() {
        let mgr = mgr_with(
            make_config(vec![
                allow_rule("cd *"),
                allow_rule("cargo *"),
                deny_rule("rm *"),
            ]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check_multi(
                &ToolKey::native("bash"),
                &["cd /tmp", "cargo test", "rm -rf /"],
                false,
                None
            ),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn complex_constructs_force_prompt_even_with_allow_star() {
        let mgr = mgr_with(make_config(vec![allow_rule("*")]), PathBuf::from("/tmp"));
        assert!(matches!(
            mgr.check_multi(&ToolKey::native("bash"), &["echo $(whoami)"], true, None),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test_case("write", "/tmp/file.txt" => true ; "write_in_cwd")]
    #[test_case("write", "/etc/passwd" => false ; "write_outside_cwd")]
    #[test_case("task", "task:research" => true ; "task_allowed")]
    #[test_case("bash", "cargo test" => false ; "bash_prompts")]
    fn builtin_check(tool: &str, scope: &str) -> bool {
        matches!(
            default_mgr().check(&ToolKey::native(tool), scope, None),
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
    fn scope_matches_relative_pattern_before_dir_exists() {
        // A relative rule like `dist/**` must match an absolute value even
        // before the directory exists.
        let cwd = std::env::current_dir().unwrap();
        let value = cwd.join("__maki_nonexistent_dist/file.txt");
        assert!(!value.exists(), "test dir must not exist");
        assert!(
            scope_matches("__maki_nonexistent_dist/**", &value.to_string_lossy()),
            "relative pattern should match absolute value: value={}",
            value.display()
        );
    }

    #[test]
    #[cfg(unix)]
    fn scope_matches_symlinked_parent_with_nonexistent_tail() {
        // Regression: symlinked leading component plus a non-existent tail
        // (`proj`). Both sides must resolve the symlink before appending the
        // lexical tail, else the prefix stays lexical and this returns false.
        let tmp = std::env::temp_dir();
        let real = tmp.join("__maki_test_scope_symlink_tail_real");
        let link = tmp.join("__maki_test_scope_symlink_tail_link");
        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // `proj` under the symlink does not exist.
        let pattern = format!("{}/proj/**", link.display());
        let value = format!("{}/proj/file.txt", link.display());
        assert!(
            scope_matches(&pattern, &value),
            "symlinked parent with non-existent tail should match: pattern={pattern}, value={value}"
        );

        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
    }

    #[test]
    fn path_traversal_prompts() {
        let path = normalize_scope_path("/tmp/../etc/passwd");
        assert!(matches!(
            default_mgr().check(&ToolKey::native("write"), &path, None),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn session_rule_overrides_config() {
        let mgr = mgr_with(
            make_config(vec![allow_rule("cargo *")]),
            PathBuf::from("/tmp"),
        );
        mgr.add_session_rule(deny_rule("cargo *"));
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test", None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn deny_overrides_default_allow() {
        let mgr = mgr_with(
            PermissionsConfig {
                default: DefaultEffect::Allow,
                rules: vec![deny_rule("rm *")],
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "rm -rf /", None),
            PermissionCheck::Denied
        ));
    }

    // When you allow "cargo test", we generalize to "cargo *" for convenience.
    // But denies stay exact, you probably have a good reason to block that specific thing.
    #[test]
    fn allow_decision_generalizes() {
        let mgr = default_mgr();
        mgr.apply_decision(
            &ToolKey::native("bash"),
            &["cargo test --all".into()],
            &PermissionAnswer::AllowSession,
        );
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo build", None),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn deny_decision_uses_exact() {
        let mgr = default_mgr();
        mgr.apply_decision(
            &ToolKey::native("bash"),
            &["cargo test".into()],
            &PermissionAnswer::DenyAlwaysLocal,
        );
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test", None),
            PermissionCheck::Denied
        ));
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo build", None),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn boundary_inside_proceeds() {
        let tmp = std::env::temp_dir();
        let mgr = mgr_with(PermissionsConfig::default(), tmp.clone());
        assert!(
            mgr.boundary_block_reason(&tmp.join("some_file.txt"))
                .is_none()
        );
    }

    #[test]
    fn boundary_outside_proceeds_via_prompt() {
        let tmp = std::env::temp_dir();
        let mgr = mgr_with(PermissionsConfig::default(), tmp);
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
        let mgr = mgr_with(PermissionsConfig::default(), sub.clone());
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
        let mgr = mgr_with(PermissionsConfig::default(), project.clone());
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
        let mgr = mgr_with(PermissionsConfig::default(), missing.clone());
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
        let mgr = mgr_with(
            make_config(vec![allow_rule("cargo *"), allow_rule("git *")]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check_multi(
                &ToolKey::native("bash"),
                &["cargo test", "git push"],
                false,
                None
            ),
            PermissionCheck::Allowed
        ));
        match mgr.check_multi(
            &ToolKey::native("bash"),
            &["cargo test", "git push"],
            true,
            None,
        ) {
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
        let mgr = mgr_with(make_config(vec![deny_rule("rm *")]), PathBuf::from("/tmp"));
        assert!(matches!(
            mgr.check_multi(&ToolKey::native("bash"), &["rm -rf /"], true, None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn check_multi_partial_coverage_prompts_uncovered() {
        let mgr = mgr_with(
            make_config(vec![allow_rule("cargo *")]),
            PathBuf::from("/tmp"),
        );
        match mgr.check_multi(
            &ToolKey::native("bash"),
            &["cargo test", "git push", "ls"],
            false,
            None,
        ) {
            PermissionCheck::NeedsPrompt { scopes, .. } => {
                assert_eq!(scopes, vec!["git push", "ls"]);
            }
            other => panic!("expected NeedsPrompt, got {other:?}"),
        }
    }

    /// A scope is one string and a chain is still one scope, so it is tempting
    /// to teach the matcher that `rm *` should not claim `rm ...; sudo ...`.
    /// Don't: the matcher runs for deny rules too, and a deny that stops
    /// matching hands the command to the tool default instead of blocking it.
    /// Chains get split by the bash plugin, before they ever reach here.
    #[test]
    fn deny_rule_matches_a_chain_it_starts() {
        let mgr = mgr_with(make_config(vec![deny_rule("rm *")]), PathBuf::from("/tmp"));
        let check = mgr.check(&ToolKey::native("bash"), "rm -rf /tmp; sudo x", None);
        assert!(matches!(check, PermissionCheck::Denied), "got {check:?}");
    }

    #[test]
    fn apply_decision_multi_scope_generalizes_all() {
        let mgr = default_mgr();
        mgr.apply_decision(
            &ToolKey::native("bash"),
            &["cargo test".into(), "git status".into()],
            &PermissionAnswer::AllowSession,
        );
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo build", None),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "git push", None),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn generalized_scopes_deduplicates() {
        let scopes = vec!["cargo test".into(), "cargo build".into()];
        let result = generalized_scopes(&ToolKey::native("bash"), &scopes);
        assert_eq!(result, vec!["cargo *"]);
    }

    #[test]
    fn generalized_scopes_preserves_distinct() {
        let scopes = vec!["cargo test".into(), "git status".into()];
        let result = generalized_scopes(&ToolKey::native("bash"), &scopes);
        assert_eq!(result, vec!["cargo *", "git *"]);
    }

    #[test_case("webfetch", "some:scope" => "some:scope" ; "unknown_tool_preserves_exact")]
    #[test_case("myserver.fetch", "{\"url\":\"https://a\"}" => "*" ; "mcp_tool_generalizes_to_wildcard")]
    fn generalize_single_scope(tool: &str, scope: &str) -> String {
        generalized_scopes(&ToolKey::parse(tool).unwrap(), &[scope.into()])
            .into_iter()
            .next()
            .unwrap()
    }

    #[test]
    fn generalize_edit_uses_parent_dir() {
        let result = generalize_scope(&ToolKey::native("edit"), "/home/user/project/src/main.rs");
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
        let result = generalize_scope(&ToolKey::native("edit"), "/Cargo.toml");
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
    #[test_case("myfetch.search", "{\"url\":\"https://a\"}" ; "mcp_tool_call")]
    fn command_matches_its_own_generalized_rule(tool: &str, scope: &str) {
        let tool_key = ToolKey::parse(tool).unwrap();
        let rule = &generalized_scopes(&tool_key, &[scope.into()])[0];
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
            &ToolKey::parse("myfetch.search").unwrap(),
            &["{\"url\":\"https://a\"}".into()],
            &PermissionAnswer::AllowSession,
        );
        // Same tool, different arguments -> allowed without reprompting.
        assert!(matches!(
            mgr.check(
                &ToolKey::parse("myfetch.search").unwrap(),
                "{\"url\":\"https://b\"}",
                None
            ),
            PermissionCheck::Allowed
        ));
        // A distinct MCP tool is not covered by the fetch rule.
        assert!(!matches!(
            mgr.check(
                &ToolKey::parse("myfetch.exec").unwrap(),
                "{\"cmd\":\"ls\"}",
                None
            ),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn deny_rule_with_none_scope_blocks_everything() {
        let mgr = mgr_with(
            make_config(vec![PermissionRule {
                tool: ToolKey::native("bash"),
                scope: None,
                effect: Effect::Deny,
            }]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "anything", None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn wildcard_deny_blocks_all_tools() {
        let mgr = mgr_with(
            make_config(vec![PermissionRule {
                tool: ToolKey::Wildcard,
                scope: None,
                effect: Effect::Deny,
            }]),
            PathBuf::from("/tmp"),
        );
        // Any deny wins: Wildcard deny blocks everything including builtins
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "ls", None),
            PermissionCheck::Denied
        ));
        assert!(matches!(
            mgr.check(&ToolKey::native("write"), "/tmp/x", None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn mcp_deny_always_blocks_all_arguments() {
        let mgr = mgr_with(make_config(vec![]), PathBuf::from("/tmp"));
        let tool = ToolKey::McpTool {
            server: "deepwiki".into(),
            tool: "search".into(),
        };
        // User denies with specific arguments — should generalize to block all.
        mgr.apply_decision(
            &tool,
            &["{\"q\":\"dangerous\"}".into()],
            &PermissionAnswer::DenyAlwaysLocal,
        );
        // Different arguments: still denied.
        assert!(matches!(
            mgr.check(&tool, "{\"q\":\"safe\"}", None),
            PermissionCheck::Denied
        ));
        // Even wildcard scope: denied.
        assert!(matches!(
            mgr.check(&tool, "*", None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn yolo_mode_allows_but_deny_still_blocks() {
        let mgr = mgr_with(make_config(vec![deny_rule("rm *")]), PathBuf::from("/tmp"));
        mgr.toggle_yolo();
        assert!(mgr.is_yolo());
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test", None),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "rm -rf /", None),
            PermissionCheck::Denied
        ));
    }

    fn seeded_mgr(yolo: bool) -> PermissionManager {
        mgr_with(
            PermissionsConfig {
                yolo,
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        )
    }

    /// A fork runs the same session, so it has to answer both questions the
    /// same way or a respawned agent drifts from the tab that owns it.
    fn yolo_state(mgr: &PermissionManager) -> (bool, Option<bool>) {
        let forked = mgr.fork();
        assert_eq!(
            (forked.is_yolo(), forked.persisted_yolo()),
            (mgr.is_yolo(), mgr.persisted_yolo()),
        );
        (mgr.is_yolo(), mgr.persisted_yolo())
    }

    /// A stored intent replaces the seed outright, and no stored intent falls
    /// back to it: `--yolo` must neither be erased by an untouched session nor
    /// survive one the user explicitly turned off.
    #[test_case(false, None        => (false, None)        ; "no_flag_and_no_intent_stays_off")]
    #[test_case(true,  None        => (true,  None)        ; "the_flag_applies_but_is_never_stored")]
    #[test_case(false, Some(true)  => (true,  Some(true))  ; "stored_on_comes_back_without_the_flag")]
    #[test_case(true,  Some(true)  => (true,  Some(true))  ; "the_flag_does_not_wipe_stored_on")]
    #[test_case(true,  Some(false) => (false, Some(false)) ; "stored_off_overrides_the_flag")]
    #[test_case(false, Some(false) => (false, Some(false)) ; "stored_off_stays_off")]
    fn a_stored_yolo_intent_replaces_the_seed(
        seed: bool,
        stored: Option<bool>,
    ) -> (bool, Option<bool>) {
        let mgr = seeded_mgr(seed);
        mgr.set_session_yolo(stored);
        yolo_state(&mgr)
    }

    /// `/yolo` always drives the effective state, so under `--yolo` it can turn
    /// the session off, and either way the session now owns the answer.
    #[test_case(false => (true,  Some(true))  ; "toggling_on_claims_the_session")]
    #[test_case(true  => (false, Some(false)) ; "toggling_off_under_the_flag_claims_the_session")]
    fn toggling_yolo_records_the_intent(seed: bool) -> (bool, Option<bool>) {
        let mgr = seeded_mgr(seed);
        assert_eq!(mgr.toggle_yolo(), !seed);
        yolo_state(&mgr)
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
        mgr.apply_decision(&ToolKey::native("bash"), &["cargo test".into()], &answer);
        assert!(mgr.session_rules_snapshot().is_empty());
    }

    #[test]
    fn default_deny_blocks_unmatched() {
        let mgr = mgr_with(
            PermissionsConfig {
                default: DefaultEffect::Deny,
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test", None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn default_deny_with_allow_rules() {
        let mgr = mgr_with(
            PermissionsConfig {
                default: DefaultEffect::Deny,
                rules: vec![allow_rule("cargo *")],
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test", None),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "rm -rf /", None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn default_allow_allows_unmatched() {
        let mgr = mgr_with(
            PermissionsConfig {
                default: DefaultEffect::Allow,
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test", None),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn default_prompt_is_default_behavior() {
        let mgr = mgr_with(PermissionsConfig::default(), PathBuf::from("/tmp"));
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test", None),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    #[test]
    fn mcp_server_wildcard_matches_all_server_tools() {
        let mgr = mgr_with(
            make_config(vec![PermissionRule {
                tool: ToolKey::McpServer {
                    server: "deepwiki".into(),
                },
                scope: None,
                effect: Effect::Allow,
            }]),
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check(
                &ToolKey::McpTool {
                    server: "deepwiki".into(),
                    tool: "search".into()
                },
                "{}",
                None
            ),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check(
                &ToolKey::McpTool {
                    server: "deepwiki".into(),
                    tool: "web_search".into()
                },
                "{}",
                None
            ),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn mcp_server_wildcard_does_not_match_other_server() {
        let mgr = mgr_with(
            make_config(vec![PermissionRule {
                tool: ToolKey::McpServer {
                    server: "deepwiki".into(),
                },
                scope: None,
                effect: Effect::Allow,
            }]),
            PathBuf::from("/tmp"),
        );
        assert!(!matches!(
            mgr.check(
                &ToolKey::McpTool {
                    server: "github".into(),
                    tool: "search".into()
                },
                "{}",
                None
            ),
            PermissionCheck::Allowed
        ));
    }

    #[test]
    fn per_tool_default_overrides_global() {
        let mgr = mgr_with(
            PermissionsConfig {
                default: DefaultEffect::Deny,
                tool_defaults: HashMap::from([(ToolKey::native("bash"), DefaultEffect::Allow)]),
                rules: vec![],
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        assert!(matches!(
            mgr.check(&ToolKey::native("bash"), "cargo test", None),
            PermissionCheck::Allowed
        ));
        assert!(matches!(
            mgr.check(&ToolKey::native("write"), "/etc/passwd", None),
            PermissionCheck::Denied
        ));
    }

    #[test_case("write", true ; "write_tool_allowed")]
    #[test_case("edit", true ; "edit_tool_allowed")]
    #[test_case("bash", false ; "non_write_tool_prompts")]
    fn plan_path_auto_allows_file_write_tools_only(tool: &str, expect_allowed: bool) {
        let plan_path = Path::new(PLAN_FILE);
        let mgr = default_mgr();
        assert_eq!(
            matches!(
                mgr.check(&ToolKey::native(tool), PLAN_FILE, Some(plan_path)),
                PermissionCheck::Allowed
            ),
            expect_allowed,
        );
    }

    /// Plan mode is read-only and an MCP server can write without saying so,
    /// so approving one for the user would break that. Native tools are known,
    /// and the ones plan mode does not block stay automatic.
    #[test_case(MCP_TOOL,  MCP_ARGS,  DefaultEffect::Prompt, true  => PROMPTS ; "yolo_plan_mode_prompts_for_mcp")]
    #[test_case(MCP_TOOL,  MCP_ARGS,  DefaultEffect::Allow,  true  => PROMPTS ; "default_allow_plan_mode_prompts_for_mcp")]
    #[test_case(MCP_TOOL,  MCP_ARGS,  DefaultEffect::Prompt, false => ALLOWED ; "yolo_build_mode_allows_mcp")]
    #[test_case(READ_TOOL, READ_SCOPE, DefaultEffect::Prompt, true => ALLOWED ; "yolo_plan_mode_allows_native_read")]
    fn plan_mode_outranks_blanket_approval_for_mcp_tools(
        tool: &str,
        scope: &str,
        default: DefaultEffect,
        plan_mode: bool,
    ) -> &'static str {
        let mgr = mgr_with(
            PermissionsConfig {
                yolo: true,
                default,
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        );
        let plan_path = Path::new(PLAN_FILE);
        outcome(mgr.check(
            &ToolKey::parse(tool).expect("test tool key parses"),
            scope,
            plan_mode.then_some(plan_path),
        ))
    }

    /// A rule is the user's own decision about this exact tool, so plan mode
    /// leaves it alone. Only automatic approval is held back.
    #[test]
    fn plan_mode_keeps_an_explicit_allow_rule_for_an_mcp_tool() {
        let mgr = mgr_with(
            make_config(vec![PermissionRule {
                tool: ToolKey::parse(MCP_TOOL).expect("test tool key parses"),
                scope: Some("*".into()),
                effect: Effect::Allow,
            }]),
            PathBuf::from("/tmp"),
        );
        assert_eq!(
            outcome(mgr.check(
                &ToolKey::parse(MCP_TOOL).expect("test tool key parses"),
                MCP_ARGS,
                Some(Path::new(PLAN_FILE)),
            )),
            ALLOWED
        );
    }

    /// The gate itself, apart from any one source of approval. A tool plan mode
    /// cannot read is the only case where how squarely a yes was aimed changes
    /// the answer.
    #[test_case(&mcp_tool_key(), true,  Approval::Standing    => false ; "opaque_tool_refuses_a_standing_yes")]
    #[test_case(&mcp_tool_key(), true,  Approval::ForThisTool => true  ; "opaque_tool_takes_an_answer_about_itself")]
    #[test_case(&mcp_tool_key(), false, Approval::Standing    => true  ; "no_plan_mode_takes_anything")]
    #[test_case(&native_key(),   true,  Approval::Standing    => true  ; "native_tool_is_never_opaque")]
    fn the_gate_only_ranks_approvals_for_a_tool_plan_mode_cannot_read(
        tool: &ToolKey,
        plan_mode: bool,
        approval: Approval,
    ) -> bool {
        ApprovalGate::new(tool, plan_mode.then_some(Path::new(PLAN_FILE))).accepts(approval)
    }

    /// A rule that was not written for this exact tool is a standing approval
    /// the user never gave it, so plan mode holds it back the way it holds back
    /// yolo. Denies are not approvals and keep winning.
    #[test_case(ToolKey::Wildcard,  Effect::Allow, true  => PROMPTS ; "wildcard_allow_prompts_in_plan_mode")]
    #[test_case(mcp_server_key(),   Effect::Allow, true  => PROMPTS ; "server_allow_prompts_in_plan_mode")]
    #[test_case(mcp_tool_key(),     Effect::Allow, true  => ALLOWED ; "exact_tool_allow_decides_in_plan_mode")]
    #[test_case(ToolKey::Wildcard,  Effect::Deny,  true  => DENIED  ; "wildcard_deny_denies_in_plan_mode")]
    #[test_case(ToolKey::Wildcard,  Effect::Allow, false => ALLOWED ; "wildcard_allow_outside_plan_mode")]
    #[test_case(mcp_server_key(),   Effect::Allow, false => ALLOWED ; "server_allow_outside_plan_mode")]
    #[test_case(ToolKey::Wildcard,  Effect::Deny,  false => DENIED  ; "wildcard_deny_outside_plan_mode")]
    fn plan_mode_holds_back_rules_not_written_for_the_mcp_tool(
        rule_key: ToolKey,
        effect: Effect,
        plan_mode: bool,
    ) -> &'static str {
        let mgr = mgr_with(
            make_config(vec![PermissionRule {
                tool: rule_key,
                scope: None,
                effect,
            }]),
            PathBuf::from(TEST_CWD),
        );
        outcome(mgr.check(
            &mcp_tool_key(),
            MCP_ARGS,
            plan_mode.then_some(Path::new(PLAN_FILE)),
        ))
    }

    #[test]
    fn plugin_rules_apply_to_manager_and_forks() {
        let store = Arc::new(PluginRuleStore::default());
        let mgr = PermissionManager::new(
            PermissionsConfig::default(),
            PathBuf::from("/tmp"),
            Arc::clone(&store),
        );
        let fork = mgr.fork();
        store.replace("memory", vec![plugin_edit_rule("/x/**", Effect::Allow)]);
        for m in [&mgr, &fork] {
            assert!(matches!(
                m.check(&ToolKey::native("edit"), "/x/f", None),
                PermissionCheck::Allowed
            ));
        }
    }

    #[test]
    fn config_deny_beats_plugin_allow() {
        let store = Arc::new(PluginRuleStore::default());
        store.replace("memory", vec![plugin_edit_rule("/x/**", Effect::Allow)]);
        let mgr = PermissionManager::new(
            make_config(vec![plugin_edit_rule("/x/**", Effect::Deny)]),
            PathBuf::from("/tmp"),
            store,
        );
        assert!(matches!(
            mgr.check(&ToolKey::native("edit"), "/x/f", None),
            PermissionCheck::Denied
        ));
    }

    #[test]
    fn plan_path_multi_scope_all_must_match() {
        let plan_path = Path::new(PLAN_FILE);
        let mgr = default_mgr();

        // All scopes match plan → allowed
        assert!(matches!(
            mgr.check_multi(
                &ToolKey::native("write"),
                &[PLAN_FILE, PLAN_FILE],
                false,
                Some(plan_path),
            ),
            PermissionCheck::Allowed
        ));

        // One scope is non-plan → needs prompt
        assert!(matches!(
            mgr.check_multi(
                &ToolKey::native("write"),
                &[PLAN_FILE, "/etc/passwd"],
                false,
                Some(plan_path),
            ),
            PermissionCheck::NeedsPrompt { .. }
        ));
    }

    mod reviewer_chain {
        use super::*;
        use crate::reviewers::LinkCallResult;
        use maki_providers::TokenUsage;
        use maki_providers::provider::BoxFuture;
        use std::collections::VecDeque;

        const SCOPE: &str = "rm -rf build";
        const TEST_BILLED: f64 = 0.001;

        struct ScriptedTransport {
            responses: Mutex<VecDeque<Result<String, String>>>,
            calls: Mutex<Vec<(String, String)>>,
        }

        impl ScriptedTransport {
            fn new(responses: &[Result<&str, &str>]) -> Arc<Self> {
                Arc::new(Self {
                    responses: Mutex::new(
                        responses
                            .iter()
                            .map(|r| r.map(str::to_owned).map_err(str::to_owned))
                            .collect(),
                    ),
                    calls: Mutex::new(Vec::new()),
                })
            }

            fn calls(&self) -> Vec<(String, String)> {
                self.calls.lock().unwrap().clone()
            }
        }

        impl ReviewTransport for ScriptedTransport {
            fn call<'a>(
                &'a self,
                spec: &'a str,
                _system: &'a str,
                user: &'a str,
                _timeouts: Timeouts,
            ) -> BoxFuture<'a, LinkCallResult> {
                self.calls
                    .lock()
                    .unwrap()
                    .push((spec.to_owned(), user.to_owned()));
                let next = self.responses.lock().unwrap().pop_front();
                Box::pin(async move {
                    match next {
                        Some(Ok(text)) => LinkCallResult {
                            text: Ok(text),
                            usage: TokenUsage {
                                input: 10,
                                output: 2,
                                ..Default::default()
                            },
                            billed_cost: Some(TEST_BILLED),
                            list_cost: None,
                        },
                        Some(Err(e)) => LinkCallResult::failed(e),
                        None => LinkCallResult::failed("script exhausted".into()),
                    }
                })
            }
        }

        fn reviewer(name: &str, tools: &[&str]) -> ReviewerDef {
            ReviewerDef {
                name: Arc::from(name),
                link: Arc::new(crate::reviewers::ModelLink {
                    spec: "test/model".into(),
                    policy: "policy".into(),
                }),
                tools: tools.iter().map(|s| (*s).to_owned()).collect(),
                timeout_ms: 1_000,
                order: 0,
                redirect_guidance: None,
            }
        }

        fn manager(
            config: PermissionsConfig,
            defs: Vec<ReviewerDef>,
            transport: Arc<ScriptedTransport>,
        ) -> PermissionManager {
            let store = Arc::new(PluginRuleStore::default());
            store.replace_reviewers("test", defs);
            PermissionManager::new(config, PathBuf::from("/tmp"), store).with_transport(transport)
        }

        fn enforce(
            mgr: &PermissionManager,
        ) -> (Result<(), PermissionError>, Vec<crate::AgentEvent>) {
            let (tx, rx) = flume::unbounded();
            let event_tx = EventSender::new(tx, 0);
            let scopes = crate::tools::PermissionScopes {
                scopes: vec![SCOPE.to_owned()],
                force_prompt: false,
            };
            let result = smol::block_on(mgr.enforce(
                &ToolKey::native("bash"),
                &scopes,
                &event_tx,
                None,
                "req-1",
                &crate::CancelToken::none(),
                None,
                ReviewSource::none(),
            ));
            let events = rx.drain().map(|envelope| envelope.event).collect();
            (result, events)
        }

        fn resolutions(events: &[crate::AgentEvent]) -> Vec<String> {
            events
                .iter()
                .filter_map(|event| match event {
                    crate::AgentEvent::ReviewerVerdict(v) => Some(v.resolution.clone()),
                    _ => None,
                })
                .collect()
        }

        #[test]
        fn allow_verdict_allows_and_reports_spend() {
            let transport = ScriptedTransport::new(&[Ok("ALLOW: read only")]);
            let mgr = manager(
                PermissionsConfig::default(),
                vec![reviewer("cheap", &["*"])],
                Arc::clone(&transport),
            );
            let (result, events) = enforce(&mgr);
            assert!(result.is_ok());
            assert_eq!(resolutions(&events), ["allowed"]);
            let crate::AgentEvent::ReviewerVerdict(v) = &events[0] else {
                panic!("expected verdict event");
            };
            assert_eq!(v.billed_cost, Some(TEST_BILLED));
            assert_eq!(v.reviewer, "cheap");
        }

        #[test]
        fn deny_verdict_hard_denies_and_never_escalates() {
            let transport = ScriptedTransport::new(&[Ok("DENY: touches prod")]);
            let mgr = manager(
                PermissionsConfig::default(),
                vec![reviewer("cheap", &["*"]), reviewer("strong", &["*"])],
                Arc::clone(&transport),
            );
            let (result, events) = enforce(&mgr);
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("denied by reviewer cheap: touches prod"),
                "{err}"
            );
            assert_eq!(transport.calls().len(), 1, "DENY must not escalate");
            assert_eq!(resolutions(&events), ["denied"]);
        }

        #[test]
        fn ask_garbage_and_errors_escalate_until_prompt() {
            let transport =
                ScriptedTransport::new(&[Ok("ASK"), Ok("maybe? sounds fine"), Err("timeout")]);
            let mgr = manager(
                PermissionsConfig::default(),
                vec![
                    reviewer("a", &["*"]),
                    reviewer("b", &["*"]),
                    reviewer("c", &["*"]),
                ],
                Arc::clone(&transport),
            );
            let (result, events) = enforce(&mgr);
            assert!(
                result.is_err(),
                "no response channel, so the prompt path denies"
            );
            assert_eq!(transport.calls().len(), 3);
            assert_eq!(
                resolutions(&events),
                ["escalated", "escalated", "escalated", "prompted"]
            );
        }

        #[test]
        fn yolo_with_matching_reviewer_redirects_instead_of_allowing() {
            let transport = ScriptedTransport::new(&[Ok("ASK"), Ok("ASK"), Ok("ASK")]);
            let config = PermissionsConfig {
                yolo: true,
                ..Default::default()
            };
            let mgr = manager(
                config,
                vec![reviewer("cheap", &["*"])],
                Arc::clone(&transport),
            );

            let (result, events) = enforce(&mgr);
            let err = result.unwrap_err().to_string();
            assert!(err.contains(REDIRECT_GUIDANCE), "{err}");
            assert_eq!(resolutions(&events), ["escalated", "redirected"]);

            let (result, _) = enforce(&mgr);
            assert!(result.unwrap_err().to_string().contains(REDIRECT_GUIDANCE));
            let (result, _) = enforce(&mgr);
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains(FINAL_REDIRECT_GUIDANCE),
                "third redirect in a turn must harden the guidance: {err}"
            );

            mgr.reset_review_turn();
            let transport2 = ScriptedTransport::new(&[Ok("ASK")]);
            let mgr2 = manager(
                PermissionsConfig {
                    yolo: true,
                    ..Default::default()
                },
                vec![reviewer("cheap", &["*"])],
                transport2,
            );
            let (result, _) = enforce(&mgr2);
            assert!(result.unwrap_err().to_string().contains(REDIRECT_GUIDANCE));
        }

        #[test]
        fn yolo_without_matching_reviewer_still_allows_all() {
            let transport = ScriptedTransport::new(&[]);
            let config = PermissionsConfig {
                yolo: true,
                ..Default::default()
            };
            let mgr = manager(
                config,
                vec![reviewer("writes", &["write"])],
                Arc::clone(&transport),
            );
            let (result, events) = enforce(&mgr);
            assert!(result.is_ok());
            assert!(transport.calls().is_empty());
            assert!(resolutions(&events).is_empty());
        }

        #[test]
        fn static_deny_rule_wins_before_any_review() {
            let transport = ScriptedTransport::new(&[Ok("ALLOW")]);
            let config = PermissionsConfig {
                yolo: true,
                rules: vec![deny_rule("rm *")],
                ..Default::default()
            };
            let mgr = manager(
                config,
                vec![reviewer("cheap", &["*"])],
                Arc::clone(&transport),
            );
            let (result, _) = enforce(&mgr);
            assert!(result.is_err());
            assert!(transport.calls().is_empty());
        }

        #[test]
        fn static_allow_rule_short_circuits_before_review() {
            let transport = ScriptedTransport::new(&[Ok("DENY")]);
            let config = make_config(vec![allow_rule("rm *")]);
            let mgr = manager(
                config,
                vec![reviewer("cheap", &["*"])],
                Arc::clone(&transport),
            );
            let (result, _) = enforce(&mgr);
            assert!(result.is_ok());
            assert!(transport.calls().is_empty());
        }

        #[test]
        fn tool_filter_scopes_the_chain() {
            let transport = ScriptedTransport::new(&[Ok("ALLOW")]);
            let mgr = manager(
                PermissionsConfig::default(),
                vec![reviewer("writes", &["write"])],
                Arc::clone(&transport),
            );
            let (result, events) = enforce(&mgr);
            assert!(result.is_err(), "bash has no reviewer, prompt path denies");
            assert!(transport.calls().is_empty());
            assert!(resolutions(&events).is_empty());
        }

        #[test]
        fn attempt_history_appears_on_the_second_review() {
            let transport = ScriptedTransport::new(&[Ok("ASK"), Ok("ASK")]);
            let mgr = manager(
                PermissionsConfig::default(),
                vec![reviewer("cheap", &["*"])],
                Arc::clone(&transport),
            );
            let _ = enforce(&mgr);
            let _ = enforce(&mgr);
            let calls = transport.calls();
            assert!(!calls[0].1.contains("Attempt history"));
            assert!(calls[1].1.contains("Attempt history"));
            assert!(calls[1].1.contains("attempt 2"));
        }

        #[test]
        fn fork_shares_the_reviewer_store() {
            let transport = ScriptedTransport::new(&[]);
            let mgr = manager(
                PermissionsConfig::default(),
                vec![reviewer("cheap", &["*"])],
                transport,
            );
            let fork = mgr.fork();
            assert!(fork.plugin_rules.has_reviewers("bash"));
            assert!(Arc::ptr_eq(&mgr.plugin_rules, &fork.plugin_rules));
        }

        #[test]
        fn chain_orders_by_order_then_plugin_then_index() {
            let store = PluginRuleStore::default();
            let mut early = reviewer("early", &["*"]);
            early.order = -1;
            store.replace_reviewers("zeta", vec![reviewer("z1", &["*"]), early]);
            store.replace_reviewers("alpha", vec![reviewer("a1", &["*"])]);
            let names: Vec<String> = store
                .reviewer_chain("bash")
                .into_iter()
                .map(|def| def.name.to_string())
                .collect();
            assert_eq!(names, ["early", "a1", "z1"]);
        }

        #[test]
        fn chain_cap_rejects_additions_beyond_the_limit() {
            let store = PluginRuleStore::default();
            for i in 0..MAX_REVIEWER_CHAIN {
                store
                    .add_reviewer("p", reviewer(&format!("r{i}"), &["bash"]))
                    .expect("under cap");
            }
            let err = store
                .add_reviewer("p", reviewer("overflow", &["bash"]))
                .expect_err("cap must reject the ninth registration");
            assert_eq!(err.tool, "bash");
            let msg = err.to_string();
            assert!(msg.contains("MAX_REVIEWER_CHAIN"), "{msg}");
        }

        #[test]
        fn chain_cap_allows_upsert_at_the_limit() {
            let store = PluginRuleStore::default();
            for i in 0..MAX_REVIEWER_CHAIN {
                store
                    .add_reviewer("p", reviewer(&format!("r{i}"), &["bash"]))
                    .expect("under cap");
            }
            store
                .add_reviewer("p", reviewer("r0", &["bash"]))
                .expect("same-name replace stays within cap");
        }

        #[test]
        fn ledger_resets_between_turns() {
            let transport = ScriptedTransport::new(&[Ok("ASK"), Ok("ASK")]);
            let mgr = manager(
                PermissionsConfig::default(),
                vec![reviewer("cheap", &["*"])],
                Arc::clone(&transport),
            );
            let _ = enforce(&mgr);
            mgr.reset_review_turn();
            let _ = enforce(&mgr);
            let calls = transport.calls();
            assert!(
                !calls[1].1.contains("Attempt history"),
                "reset must clear the per-turn attempt ledger: {}",
                calls[1].1
            );
        }

        #[test]
        fn add_reviewer_upserts_by_name() {
            let store = PluginRuleStore::default();
            store
                .add_reviewer("p", reviewer("cheap", &["*"]))
                .expect("first add fits");
            let mut replacement = reviewer("cheap", &["write"]);
            replacement.link = Arc::new(crate::reviewers::ModelLink {
                spec: "other/model".into(),
                policy: "policy".into(),
            });
            store
                .add_reviewer("p", replacement)
                .expect("upsert never overflows");
            let chain = store.reviewer_chain("write");
            assert_eq!(chain.len(), 1);
            assert_eq!(chain[0].link.label(), "other/model");
            assert!(store.reviewer_chain("bash").is_empty());
        }
    }
}
