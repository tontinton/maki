//! Default child-side workload: the execution engine inside the sandbox.
//!
//! Assembles the tool map used both by parent-initiated tool calls and by
//! code running in the interpreter: bash (Rust), filesystem tools backed by
//! Lua plugins loaded from the sandboxed plugin dir, and trusted forwards.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use maki_agent::agent::UNKNOWN_TOOL_PREFIX;
use maki_interpreter::runner::{self, ToolFn};
use maki_sandbox::{ChildIoResult, register_child_workload};
use maki_sandbox::workload::{ChildCtx, ChildSession, ChildWorkload, RunSpec};
use serde_json::Value;
use tracing::{debug, warn};

use crate::child_lua::{ChildLuaRuntime, HostForward};

/// Tools that must run in the parent process (network, UI, agent state).
/// All other tools are executed by local functions inside the sandbox.
const TRUSTED_TOOLS: &[&str] = &[
    "webfetch",
    "websearch",
    "question",
    "todo_write",
    "task",
    "memory",
    "skill",
    "index",
];

/// Filesystem tools executed by the child-side Lua plugins. The plugins run
/// inside the mount namespace, so their operations are naturally sandboxed.
const CHILD_LOCAL_TOOLS: &[&str] = &["read", "write", "edit", "multiedit", "glob", "grep", "list"];

/// Sandbox-side config dirs probed for user plugins, in precedence order.
/// `~/.config/maki` is the XDG layout (mounted by the `plugins` profile);
/// `~/.maki` is the legacy layout.
const SANDBOX_CONFIG_DIRS: &[&str] = &["/home/maki/.config/maki", "/home/maki/.maki"];

/// First existing `<config>/plugins` dir, or the XDG candidate when none
/// exists (`ChildLuaRuntime` falls back to the embedded plugins then).
fn discover_plugin_dir(candidates: &[&str]) -> PathBuf {
    candidates
        .iter()
        .map(|d| Path::new(d).join("plugins"))
        .find(|p| p.is_dir())
        .unwrap_or_else(|| Path::new(candidates[0]).join("plugins"))
}

/// Register the default workload with `maki-sandbox`.
///
/// Call once at process startup, before any child is spawned or re-execed.
pub fn install_child_workload() {
    if !register_child_workload(Arc::new(MakiChildWorkload)) {
        debug!("maki-tools: child workload already registered");
    }
}

struct MakiChildWorkload;

impl ChildWorkload for MakiChildWorkload {
    fn init(&self, ctx: ChildCtx) -> Result<Box<dyn ChildSession>, String> {
        let plugin_dir = discover_plugin_dir(SANDBOX_CONFIG_DIRS);
        let lua_runtime = {
            let ctx = ctx.clone();
let forward: HostForward = Arc::new(move |name, args, kwargs| {
            ctx.forward_trusted(name, args, kwargs)
        });
            match ChildLuaRuntime::with_forwarder(&plugin_dir, forward) {
                Ok(rt) => Some(Arc::new(rt)),
                Err(e) => {
                    warn!(error = %e, "sandbox child: lua runtime init failed, filesystem tools unavailable");
                    None
                }
            }
        };

        if let Some(ref rt) = lua_runtime {
            let init = SANDBOX_CONFIG_DIRS
                .iter()
                .map(|d| Path::new(d).join("init.lua"))
                .find(|p| p.is_file());
            if let Some(init) = init
                && let Err(e) = rt.run_init_file(&init)
            {
                warn!(error = %e, "sandbox child: user init.lua failed");
            }
        }

        let mut tools: HashMap<String, ToolFn> = HashMap::new();
        tools.insert("bash".into(), build_bash_tool(&ctx));
        if let Some(ref rt) = lua_runtime {
            extend_lua_tools(&mut tools, Arc::clone(rt))?;
        }
        extend_trusted_tools(&mut tools, &ctx);

        Ok(Box::new(ToolsSession {
            ctx,
            tools,
            lua_runtime,
        }))
    }
}

struct ToolsSession {
    ctx: ChildCtx,
    tools: HashMap<String, ToolFn>,
    lua_runtime: Option<Arc<ChildLuaRuntime>>,
}

impl ChildSession for ToolsSession {
    fn run_code(&mut self, spec: RunSpec) -> ChildIoResult {
        if let Some(ref rt) = self.lua_runtime
            && let Err(e) = rt.set_config(&spec.config)
        {
            warn!(error = %e, "sandbox child: failed to apply run config");
        }
        let limits = runner::limits(
            std::time::Duration::from_secs(spec.timeout_secs),
            spec.max_memory,
        );
        let ctx = self.ctx.clone();
        let call_id = spec.call_id;
        match runner::run(&spec.code, "", &self.tools, None, limits, &mut |line| {
            ctx.stream_stdout(call_id, line.to_string());
        }) {
            Ok(interp) => {
                debug!(call_id = spec.call_id, "sandbox child: run finished");
                ChildIoResult {
                    output: interp.output,
                    stdout: interp.stdout,
                    error: None,
                }
            }
            Err(e) => {
                warn!(call_id = spec.call_id, error = %e, "sandbox child: interpreter error");
                ChildIoResult {
                    output: None,
                    stdout: String::new(),
                    error: Some(format!("interpreter: {e}")),
                }
            }
        }
    }

    fn handle_tool_call(
        &mut self,
        name: &str,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<String, String> {
        match self.tools.get(name) {
            Some(tool) => tool(name, args, kwargs).map(|v| v.to_string()),
            None => Err(format!("{UNKNOWN_TOOL_PREFIX}: {name}")),
        }
    }
}

fn build_bash_tool(ctx: &ChildCtx) -> ToolFn {
    let ctx = ctx.clone();
    Box::new(
        move |_: &str, args: Vec<Value>, kwargs: Vec<(String, Value)>| {
            let command = require_str(&args, &kwargs, "command")?;
            let workdir = kwargs
                .iter()
                .find(|(k, _)| k == "workdir")
                .and_then(|(_, v)| v.as_str())
                .or_else(|| {
                    args.first()
                        .and_then(|a| a.get("workdir"))
                        .and_then(|v| v.as_str())
                });
            match ctx.exec(&command, workdir) {
                Ok((output, true)) => Err(output),
                Ok((output, false)) => Ok(Value::String(output)),
                Err(e) => Err(format!("bash failed: {e}")),
            }
        },
    )
}

/// Each registered Lua plugin becomes a `ToolFn` calling into
/// [`ChildLuaRuntime`]; only child-local names are exposed.
fn extend_lua_tools(
    tools: &mut HashMap<String, ToolFn>,
    runtime: Arc<ChildLuaRuntime>,
) -> Result<(), String> {
    let names = runtime
        .registered_tool_names()
        .map_err(|e| format!("lua_runtime: cannot list tools: {e}"))?;
    for name in names {
        if !CHILD_LOCAL_TOOLS.contains(&name.as_str()) {
            debug!(tool = %name, "extend_lua_tools: not child-local, skipping");
            continue;
        }
        let rt = Arc::clone(&runtime);
        let tool_name = name.clone();
        tools.insert(
            name,
            Box::new(
                move |_fn_name: &str, args: Vec<Value>, kwargs: Vec<(String, Value)>| match rt
                    .call_tool(&tool_name, &args, &kwargs)
                {
                    Ok((output, is_error)) => {
                        if is_error {
                            Err(output)
                        } else {
                            Ok(Value::String(output))
                        }
                    }
                    Err(e) => Err(e),
                },
            ),
        );
    }
    Ok(())
}

fn extend_trusted_tools(tools: &mut HashMap<String, ToolFn>, ctx: &ChildCtx) {
    for name in TRUSTED_TOOLS {
        let ctx = ctx.clone();
        tools.insert(
            name.to_string(),
            Box::new(move |fn_name: &str, args, kwargs| {
                ctx.forward_trusted(fn_name, args, kwargs)
                    .map(Value::String)
            }),
        );
    }
}

fn require_str(args: &[Value], kwargs: &[(String, Value)], name: &str) -> Result<String, String> {
    if let Some(val) = kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v) {
        return val
            .as_str()
            .map(String::from)
            .ok_or_else(|| format!("{name} must be a string"));
    }
    if let Some(first) = args.first() {
        if let Some(s) = first.as_str() {
            return Ok(s.to_string());
        }
        // LLM sends the whole input object as args[0]; unwrap it.
        if let Some(val) = first.get(name) {
            return val
                .as_str()
                .map(String::from)
                .ok_or_else(|| format!("{name} must be a string"));
        }
        return Err("first arg must be a string".to_string());
    }
    Err(format!("missing required argument: {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MISSING_ARG: &str = "missing required argument";

    #[test]
    fn require_str_from_kwargs() {
        let args: Vec<Value> = vec![];
        let kwargs = vec![("path".into(), json!("/foo/bar"))];
        assert_eq!(require_str(&args, &kwargs, "path").unwrap(), "/foo/bar");
    }

    #[test]
    fn require_str_from_positional() {
        let args = vec![json!("/positional")];
        let kwargs: Vec<(String, Value)> = vec![];
        assert_eq!(require_str(&args, &kwargs, "path").unwrap(), "/positional");
    }

    #[test]
    fn require_str_missing_returns_error() {
        let err = require_str(&[], &[], "path").unwrap_err();
        assert!(
            err.contains(MISSING_ARG),
            "expected missing arg error, got: {err}"
        );
    }

    #[test]
    fn require_str_non_string_returns_error() {
        let kwargs = vec![("path".into(), json!(42))];
        let err = require_str(&[], &kwargs, "path").unwrap_err();
        assert!(
            err.contains("must be a string"),
            "expected type error, got: {err}"
        );
    }

    #[test]
    fn require_str_from_object_in_args() {
        let args = vec![json!({"command": "ls -la", "workdir": "/tmp"})];
        assert_eq!(require_str(&args, &[], "command").unwrap(), "ls -la");
    }

    #[test]
    fn require_str_object_missing_key_errors() {
        let args = vec![json!({"other": "x"})];
        assert!(require_str(&args, &[], "command").is_err());
    }

    #[test]
    fn require_str_kwargs_takes_priority() {
        let args = vec![json!("/positional")];
        let kwargs = vec![("path".into(), json!("/from_kwargs"))];
        assert_eq!(require_str(&args, &kwargs, "path").unwrap(), "/from_kwargs");
    }

    #[test]
    fn discover_plugin_dir_prefers_first_existing_candidate() {
        let tmp = tempfile::TempDir::new().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir_all(first.join("plugins")).unwrap();
        std::fs::create_dir_all(second.join("plugins")).unwrap();
        let got = discover_plugin_dir(&[first.to_str().unwrap(), second.to_str().unwrap()]);
        assert_eq!(got, first.join("plugins"));
    }

    #[test]
    fn discover_plugin_dir_falls_back_to_first_candidate() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("missing");
        let got = discover_plugin_dir(&[missing.to_str().unwrap()]);
        assert_eq!(got, missing.join("plugins"));
    }
}
