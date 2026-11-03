use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use maki_agent::agent::{AgentCall, AgentSlot};
use maki_agent::tools::ToolRegistry;
use maki_agent::tools::hook::Verdict;
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::{AgentMode, CancelToken};
use maki_config::{
    ProjectConfig,
    project::{self, TrustMode},
};
use maki_lua::test_support::runtime;
use maki_lua::{PluginHost, SessionRequest, UiAction};
use maki_storage::id::MakiId;
use serde_json::json;
use tempfile::TempDir;
use test_case::test_case;

const SKILL_SOURCE: &str = include_str!("../../plugins/skill/init.lua");
const MODEL: &str = "anthropic/claude-sonnet-4";
const DEADLINE: Duration = Duration::from_secs(30);
const RELEASE_SOURCE: &str = "---\nname: release\ndescription: Cut a release\ncompatibility: Requires git\n---\nRead CHANGELOG.md first.";
const SKILL_NOT_FOUND: &str = "skill not found: ";
const DURABLE_SKILL_KEY: &str = "skill:release";
const SKILL_USAGE: &str = "Usage: /skill name [request]";

#[test]
fn skill_declares_core_retention_without_a_compaction_hook() {
    let registry = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();
    let source = format!(
        "maki.uv.cwd = function() return nil end\nmaki.uv.os_homedir = function() return nil end\nmaki.env.config_dir = function() return nil end\n{}",
        SKILL_SOURCE,
    );
    host.load_source("skill", &source).unwrap();
    let tool = registry.get("skill").unwrap();
    assert_eq!(
        tool.tool.durable_key(&json!({"name": "maki-plugin-dev"})),
        Some("maki-plugin-dev")
    );
    if let Some(hook) = registry.agent_hook() {
        let cancel = CancelToken::none();
        let call = AgentCall {
            session_id: None,
            task_id: Some("child"),
            model: MODEL,
            context_size: 0,
            context_window: 0,
            cancel: &cancel,
            deadline: Instant::now() + DEADLINE,
        };
        assert!(matches!(
            runtime().block_on(hook.run(
                AgentSlot::CompactBefore,
                json!({"reason": "auto"}),
                &call
            )),
            Verdict::Unchanged
        ));
    }
}

fn isolated_skill_host(
    has_skill: bool,
    trusted: bool,
    relative: &str,
) -> (TempDir, Arc<ToolRegistry>, PluginHost) {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join(relative).join("release");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    if has_skill {
        fs::write(skill_dir.join("SKILL.md"), RELEASE_SOURCE).unwrap();
    }
    let registry = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();
    if trusted {
        let decision = project::resolve_noninteractive(dir.path(), TrustMode::Session);
        host.set_project_config(&decision.project_config).unwrap();
    }
    let source = format!(
        "maki.uv.cwd = function() return {} end\nmaki.uv.os_homedir = function() return nil end\nmaki.env.config_dir = function() return {} end\n{}",
        serde_json::to_string(dir.path()).unwrap(),
        serde_json::to_string(&dir.path().join("personal")).unwrap(),
        SKILL_SOURCE,
    );
    host.load_source_with_opts(
        "skill",
        &source,
        json!({ "plugin_dev": false }).as_object().unwrap().clone(),
    )
    .unwrap();
    (dir, registry, host)
}

#[test_case("release include the changelog", true ; "injects_skill_before_the_model_runs")]
#[test_case("missing", false ; "unknown_skill_does_not_reach_the_model")]
#[test_case("", false ; "missing_name_does_not_start_a_prompt")]
#[test_case("release", false ; "invalidated_file_does_not_reach_the_model")]
fn direct_skill_command_uses_the_shared_loader(args: &str, loads: bool) {
    let (dir, registry, host) = isolated_skill_host(true, true, ".maki/skills");
    if args == "release" && !loads {
        fs::write(
            dir.path().join(".maki/skills/release/SKILL.md"),
            "---\n123\n---\nBody",
        )
        .unwrap();
    }
    assert!(
        host.command_reader()
            .load()
            .commands
            .iter()
            .any(|command| command.name.as_ref() == "/skill"),
        "the skill command must be registered"
    );
    let rx = host.ui_action_rx();
    host.event_handle()
        .run_command(Arc::from("skill"), Arc::from("/skill"), args.into(), 0);
    let session = MakiId::generate().to_string();
    let first = rx.recv_timeout(DEADLINE).unwrap();
    let first = if let UiAction::Session {
        req: SessionRequest::Current,
        reply_tx,
    } = first
    {
        reply_tx.send(Ok(json!(session))).unwrap();
        rx.recv_timeout(DEADLINE).unwrap()
    } else {
        first
    };
    match first {
        UiAction::Session {
            req: SessionRequest::Prompt { id, text },
            reply_tx,
        } => {
            assert_eq!(id.as_deref(), Some(session.as_str()));
            assert_eq!(text, format!("/skill {args}"));
            match user_message_verdict(&registry, &text, Some(&session), "plugin") {
                Verdict::Replaced(value) if loads => {
                    let rewritten = value["text"].as_str().unwrap();
                    assert!(rewritten.contains("compatibility: Requires git"));
                    assert!(rewritten.contains("Read CHANGELOG.md first."));
                    assert!(rewritten.contains(dir.path().to_str().unwrap()));
                    assert!(rewritten.ends_with("include the changelog"));
                    assert_eq!(value["durable"]["key"], DURABLE_SKILL_KEY);
                    assert!(
                        !value["durable"]["text"]
                            .as_str()
                            .unwrap()
                            .contains("include the changelog")
                    );
                }
                Verdict::Denied(message) if !loads => {
                    assert_eq!(message, format!("{SKILL_NOT_FOUND}{args}"))
                }
                _ => panic!("only valid skills may reach the model"),
            }
            reply_tx.send(Ok(json!("queued"))).unwrap();
        }
        UiAction::Flash(message) if args.is_empty() => assert_eq!(message, SKILL_USAGE),
        _ => panic!("unexpected skill command action"),
    }
}

#[test_case(true ; "names_are_constrained_to_the_catalog")]
#[test_case(false ; "empty_catalog_has_no_tool")]
fn skill_tool_matches_the_available_catalog(has_skill: bool) {
    let (_dir, registry, _host) = isolated_skill_host(has_skill, true, ".maki/skills");
    match registry.get("skill") {
        Some(entry) if has_skill => {
            assert_eq!(
                entry.tool.schema()["properties"]["name"]["enum"],
                json!(["release"])
            );
        }
        None if !has_skill => {}
        _ => panic!("the skill tool must match the available catalog"),
    }
}

#[test]
fn unconfigured_host_skips_project_skills() {
    let (_dir, registry, _host) = isolated_skill_host(true, false, ".maki/skills");
    assert!(
        registry.get("skill").is_none(),
        "project skills require folder trust"
    );
}

#[test_case(".maki/skills" ; "maki")]
#[test_case(".claude/skills" ; "claude")]
#[test_case(".opencode/skills" ; "opencode")]
#[test_case(".agents/skills" ; "agents")]
fn all_project_skill_aliases_require_the_same_trust(relative: &str) {
    for trusted in [false, true] {
        let (_dir, registry, _host) = isolated_skill_host(true, trusted, relative);
        assert_eq!(registry.get("skill").is_some(), trusted);
    }
}

#[test]
fn revoked_project_trust_reaches_existing_plugin_globals() {
    let (dir, registry, host) = isolated_skill_host(true, true, ".maki/skills");
    host.set_project_config(&ProjectConfig::discover(dir.path()))
        .unwrap();
    host.load_source("trust_probe", "assert(#maki.env.project_dirs() == 0)")
        .unwrap();
    let entry = registry.get("skill").unwrap();
    let invocation = entry.tool.parse(&json!({ "name": "release" })).unwrap();
    let context = stub_ctx(&AgentMode::Build);
    let result = runtime().block_on(invocation.execute(&context));
    assert_eq!(
        result.output.unwrap_err(),
        format!("{SKILL_NOT_FOUND}release")
    );
}

#[test]
fn personal_skills_remain_available_without_project_trust() {
    let (_dir, registry, _host) = isolated_skill_host(true, false, "personal/skills");
    let entry = registry
        .get("skill")
        .expect("personal skill must remain available");
    assert_eq!(
        entry.tool.schema()["properties"]["name"]["enum"],
        json!(["release"])
    );
}

fn user_message_verdict(
    registry: &ToolRegistry,
    text: &str,
    session: Option<&str>,
    source: &str,
) -> Verdict {
    let hook = registry.agent_hook().unwrap();
    let cancel = CancelToken::none();
    let call = AgentCall {
        session_id: session,
        task_id: None,
        model: MODEL,
        context_size: 0,
        context_window: 0,
        cancel: &cancel,
        deadline: Instant::now() + DEADLINE,
    };
    runtime().block_on(hook.run(
        AgentSlot::UserMessage,
        json!({ "text": text, "source": source }),
        &call,
    ))
}

#[test_case("headless", "/skill release include the changelog", Some(true) ; "headless_activation_is_resolved_before_the_model")]
#[test_case("acp", "/skill release include the changelog", Some(true) ; "acp_activation_is_resolved_before_the_model")]
#[test_case("headless", "/skill missing", Some(false) ; "headless_unknown_skill_is_rejected")]
#[test_case("headless", "/skill", Some(false) ; "headless_missing_name_is_rejected")]
#[test_case("headless", "/skills release", None ; "other_prompts_are_unchanged")]
fn skill_activation_works_through_the_shared_agent_hook(
    source: &str,
    text: &str,
    loads: Option<bool>,
) {
    let (_dir, registry, _host) = isolated_skill_host(true, true, ".maki/skills");
    match (loads, user_message_verdict(&registry, text, None, source)) {
        (Some(true), Verdict::Replaced(value)) => {
            let text = value["text"].as_str().unwrap();
            let durable = value["durable"]["text"].as_str().unwrap();
            assert!(durable.contains("Read CHANGELOG.md first."));
            assert!(text.starts_with(durable));
            assert!(text.ends_with("include the changelog"));
            assert!(!durable.contains("include the changelog"));
            assert_eq!(value["durable"]["key"], DURABLE_SKILL_KEY);
            assert_eq!(value["source"], source);
        }
        (Some(false), Verdict::Denied(message)) => {
            let expected = if text == "/skill" {
                SKILL_USAGE.to_owned()
            } else {
                format!("{SKILL_NOT_FOUND}missing")
            };
            assert_eq!(message, expected);
        }
        (None, Verdict::Unchanged) => {}
        _ => panic!("skill activation must happen before the provider request"),
    }
}

#[test_case(".maki/skills" ; "maki")]
#[test_case(".claude/skills" ; "claude")]
#[test_case(".opencode/skills" ; "opencode")]
#[test_case(".agents/skills" ; "agents")]
fn nested_project_skills_use_the_native_trust_scope(relative: &str) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("packages/app");
    let skills = cwd.join(relative).join("release");
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::create_dir_all(&skills).unwrap();
    fs::write(skills.join("SKILL.md"), RELEASE_SOURCE).unwrap();
    let registry = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();
    let decision = project::resolve_noninteractive(&cwd, TrustMode::Session);
    host.set_project_config(&decision.project_config).unwrap();
    let source = format!(
        "maki.uv.cwd = function() return nil end\nmaki.uv.os_homedir = function() return nil end\nmaki.env.config_dir = function() return nil end\n{}",
        SKILL_SOURCE,
    );
    host.load_source_with_opts(
        "skill",
        &source,
        json!({"plugin_dev": false}).as_object().unwrap().clone(),
    )
    .unwrap();
    assert_eq!(
        registry.get("skill").unwrap().tool.schema()["properties"]["name"]["enum"],
        json!(["release"])
    );
}
