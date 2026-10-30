use std::str::FromStr;

use cedar_policy::{
    Authorizer, Context, Decision, Entities, EntityUid, PolicySet, Request, Response,
    RestrictedExpression, Schema,
};

use crate::{ACTION, CTX_SCOPE, CTX_TOOL, CedarError, PRINCIPAL, TOOL_TYPE};

/// A compiled Cedar policy set ready to answer `(tool, scope)` questions.
///
/// Holds no entities: maki models the principal and action as fixed singletons
/// and carries the varying `(tool, scope)` in the request context, so every
/// decision is a pure in-process evaluation with no entity store to sync.
pub struct CedarEngine {
    authorizer: Authorizer,
    policies: PolicySet,
    entities: Entities,
    principal: EntityUid,
    action: EntityUid,
    schema: Option<Schema>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CedarDecision {
    Allow,
    Deny(String),
    NoDecision,
}

impl CedarEngine {
    /// Build from Cedar policy source text and an optional Cedar schema
    /// (human `.cedarschema` syntax). A schema is validated against but not
    /// required; without one, policies still evaluate.
    pub fn from_source(policies: &str, schema: Option<&str>) -> Result<Self, CedarError> {
        let policies =
            PolicySet::from_str(policies).map_err(|e| CedarError::Policies(e.to_string()))?;
        let schema = match schema {
            Some(src) => {
                Some(Schema::from_str(src).map_err(|e| CedarError::Schema(e.to_string()))?)
            }
            None => None,
        };
        let principal =
            EntityUid::from_str(PRINCIPAL).map_err(|e| CedarError::Request(e.to_string()))?;
        let action = EntityUid::from_str(ACTION).map_err(|e| CedarError::Request(e.to_string()))?;
        Ok(Self {
            authorizer: Authorizer::new(),
            policies,
            entities: Entities::empty(),
            principal,
            action,
            schema,
        })
    }

    /// Authorize one `(tool, scope)` pair.
    ///
    /// `tool_key` is the maki `ToolKey` display string (`bash`, `server.tool`,
    /// `*`); `scope` is one requested scope (a bash command, a path) or `None`
    /// for a scopeless tool. A matching `forbid` returns [`CedarDecision::Deny`]
    /// (deny wins, as in Cedar), a `permit` returns [`CedarDecision::Allow`],
    /// and no match returns [`CedarDecision::NoDecision`].
    pub fn decide(&self, tool_key: &str, scope: Option<&str>) -> CedarDecision {
        let resource = match self.resource(tool_key) {
            Ok(r) => r,
            Err(e) => return CedarDecision::Deny(e.to_string()),
        };
        let context = match self.context(tool_key, scope) {
            Ok(c) => c,
            Err(e) => return CedarDecision::Deny(e.to_string()),
        };
        let request = match Request::new(
            self.principal.clone(),
            self.action.clone(),
            resource,
            context,
            self.schema.as_ref(),
        ) {
            Ok(r) => r,
            Err(e) => return CedarDecision::Deny(e.to_string()),
        };

        let response = self
            .authorizer
            .is_authorized(&request, &self.policies, &self.entities);
        match response.decision() {
            Decision::Allow => CedarDecision::Allow,
            // Cedar is two-valued. An explicit forbid names the policy in the
            // diagnostics; "nothing matched" names nothing, which maki turns
            // back into a prompt via NoDecision.
            Decision::Deny if response.diagnostics().reason().next().is_some() => {
                CedarDecision::Deny(deny_reason(&response))
            }
            Decision::Deny => CedarDecision::NoDecision,
        }
    }

    fn resource(&self, tool_key: &str) -> Result<EntityUid, CedarError> {
        let uid = format!("{TOOL_TYPE}::{}", quote(tool_key));
        EntityUid::from_str(&uid).map_err(|e| CedarError::ToolKey {
            key: tool_key.to_string(),
            reason: e.to_string(),
        })
    }

    fn context(&self, tool_key: &str, scope: Option<&str>) -> Result<Context, CedarError> {
        let pairs = [
            (
                CTX_TOOL.to_string(),
                RestrictedExpression::new_string(tool_key.to_string()),
            ),
            (
                CTX_SCOPE.to_string(),
                RestrictedExpression::new_string(scope.unwrap_or("").to_string()),
            ),
        ];
        Context::from_pairs(pairs).map_err(|e| CedarError::Request(e.to_string()))
    }
}

/// A Cedar entity-id literal is a quoted string; escape `"` and `\`.
fn quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn deny_reason(response: &Response) -> String {
    let ids: Vec<String> = response
        .diagnostics()
        .reason()
        .map(|p| p.to_string())
        .collect();
    if ids.is_empty() {
        "denied by Cedar policy".to_string()
    } else {
        format!("denied by Cedar policy {}", ids.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::{CedarDecision, CedarEngine};
    use test_case::test_case;

    const ALLOW_GIT: &str = r#"permit(principal, action, resource == Tool::"bash") when { context.scope like "git *" };"#;
    const FORBID_GIT: &str = r#"forbid(principal, action, resource == Tool::"bash") when { context.scope like "git *" };"#;
    const FORBID_RM: &str = r#"forbid(principal, action, resource == Tool::"bash") when { context.scope like "rm *" };"#;
    const ALLOW_SRC: &str = r#"permit(principal, action, resource == Tool::"edit") when { context.scope like "src/*" };"#;
    const ALLOW_MCP_TOOL: &str =
        r#"permit(principal, action, resource == Tool::"deepwiki.search");"#;

    fn engine(policies: &str) -> CedarEngine {
        CedarEngine::from_source(policies, None).expect("policies compile")
    }

    fn assert_deny(d: CedarDecision) {
        match d {
            CedarDecision::Deny(_) => {}
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test_case("git status" ; "like_wildcard_matches_prefix")]
    #[test_case("git commit -m x" ; "like_wildcard_matches_longer")]
    fn permit_like_allows(scope: &str) {
        assert_eq!(
            engine(ALLOW_GIT).decide("bash", Some(scope)),
            CedarDecision::Allow
        );
    }

    #[test_case("npm install" ; "non_matching_command")]
    #[test_case("gitx" ; "prefix_without_space_does_not_match")]
    fn permit_like_no_decision_when_unmatched(scope: &str) {
        assert_eq!(
            engine(ALLOW_GIT).decide("bash", Some(scope)),
            CedarDecision::NoDecision
        );
    }

    #[test]
    fn forbid_wins_over_permit() {
        let both = format!("{ALLOW_GIT}\n{FORBID_GIT}");
        assert_deny(engine(&both).decide("bash", Some("git status")));
    }

    #[test]
    fn forbid_denies_matching_scope() {
        assert_deny(engine(FORBID_RM).decide("bash", Some("rm -rf /")));
    }

    #[test]
    fn no_policy_is_no_decision() {
        assert_eq!(
            engine(ALLOW_GIT).decide("edit", Some("src/x.rs")),
            CedarDecision::NoDecision
        );
    }

    #[test_case("src/main.rs", CedarDecision::Allow ; "path_prefix_matches")]
    #[test_case("tests/x.rs", CedarDecision::NoDecision ; "path_prefix_outside")]
    fn path_prefix_scopes(scope: &str, expected: CedarDecision) {
        assert_eq!(engine(ALLOW_SRC).decide("edit", Some(scope)), expected);
    }

    #[test]
    fn mcp_tool_exact_match_ignores_scope() {
        let e = engine(ALLOW_MCP_TOOL);
        assert_eq!(e.decide("deepwiki.search", None), CedarDecision::Allow);
        assert_eq!(e.decide("deepwiki.other", None), CedarDecision::NoDecision);
    }
}
