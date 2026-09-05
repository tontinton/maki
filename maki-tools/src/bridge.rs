//! Parent-side bridge between the plugin host and the sandbox child.
//!
//! Trusted tool calls the child forwards during a run are answered by
//! invoking the corresponding Lua tool functions from the plugin host.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_lite::future::block_on;
use maki_agent::agent::UNKNOWN_TOOL_PREFIX;
use maki_agent::tools::interpreter_bridge::build_tool_input;
use maki_interpreter::runner::InterpreterResult;
use maki_lua::{json_to_lua, lua_tool_result};
use maki_sandbox::Sandbox;
use mlua::{
    Error as LuaError, Function, Lua, MultiValue, Result as LuaResult, Table, Value as LuaValue,
};
use serde_json::Value;

use crate::child_lua::HOST_UI_PREFIX;

async fn call_lua_tool(
    lua: Lua,
    func: Function,
    name: String,
    arg: LuaValue,
) -> Result<String, String> {
    let thread = lua
        .create_thread(func)
        .map_err(|e| format!("{name}: {e}"))?;
    let values: MultiValue = thread
        .into_async(arg)
        .map_err(|e| format!("{name}: {e}"))?
        .await
        .map_err(|e| format!("{name}: {e}"))?;
    lua_tool_result(values).map_err(|e| format!("{name}: {e}"))
}

/// Answer a `maki.ui.*` call raised by the sandboxed Lua plugin: resolve the
/// named function in the host's own `maki.ui` table so the call lands in the
/// host UI (the child has no terminal of its own). Arguments stay positional
/// — a bare nil clears a plugin's status hints, exactly like the host call.
fn forward_host_ui(lua: &Lua, name: &str, args: &[Value]) -> Result<String, String> {
    let fn_name = name
        .strip_prefix(HOST_UI_PREFIX)
        .ok_or_else(|| format!("{name} is not a {HOST_UI_PREFIX} call"))?;
    let maki: Table = lua
        .globals()
        .get("maki")
        .map_err(|e| format!("host maki global missing: {e}"))?;
    let ui: Table = maki
        .get("ui")
        .map_err(|e| format!("host maki.ui missing: {e}"))?;
    let func: Function = ui
        .get(fn_name)
        .map_err(|e| format!("host maki.ui.{fn_name} missing: {e}"))?;
    let call_args = args
        .iter()
        .map(|v| json_to_lua(lua, v))
        .collect::<LuaResult<Vec<LuaValue>>>()
        .map_err(|e| e.to_string())?;
    func.call::<()>(MultiValue::from_vec(call_args))
        .map_err(|e| format!("{fn_name}: {e}"))?;
    Ok(String::new())
}

/// Run `code` inside the sandbox child, exposing the given Lua functions as
/// trusted tools.
///
/// # Errors
///
/// Returns a `LuaError` if the sandbox runtime fails to start; tool-level
/// failures are reported as the `Err(String)` in the inner `Result`.
pub async fn run_sandbox_with<S: std::hash::BuildHasher + Send + Sync + 'static>(
    sandbox: &Arc<Sandbox>,
    lua: Lua,
    code: String,
    timeout: Duration,
    fns: HashMap<String, Function, S>,
    config_json: String,
) -> Result<Result<InterpreterResult, String>, LuaError> {
    // Trusted tool calls arrive on the sandbox IO thread while the run is
    // active; this handler runs there and must not wait on that thread.
    let sandbox = Arc::clone(sandbox);
    let result = smol::unblock(move || {
        sandbox.run_code(
            code,
            timeout.as_secs(),
            0,
            config_json,
            move |name, args, kwargs| {
                if let Some(func) = fns.get(name).cloned() {
                    let input = build_tool_input(&args, &kwargs).map_err(|e| e.clone())?;
                    let arg = json_to_lua(&lua, &input).map_err(|e| e.to_string())?;
                    return block_on(call_lua_tool(lua.clone(), func, name.to_owned(), arg));
                }
                if name.starts_with(HOST_UI_PREFIX) {
                    return forward_host_ui(&lua, name, &args);
                }
                Err(format!("{UNKNOWN_TOOL_PREFIX}: {name}"))
            },
        )
    })
    .await
    .map_err(|e| LuaError::runtime(format!("sandbox run: {e}")))?;

    if let Some(err) = result.error {
        return Ok(Err(err));
    }

    Ok(Ok(InterpreterResult {
        output: result.output,
        stdout: result.stdout,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_lua::lua_to_json;
    use serde_json::json;

    #[test]
    fn forwarded_ui_calls_reach_the_host_ui_table() {
        let lua = Lua::new();
        let seen: Arc<std::sync::Mutex<Vec<(String, Value)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let forward_seen = Arc::clone(&seen);
        let maki = lua.create_table().unwrap();
        let ui = lua.create_table().unwrap();
        let hint_seen = Arc::clone(&forward_seen);
        let flash_seen = Arc::clone(&forward_seen);
        ui.set(
            "set_status_hint",
            lua.create_function(move |lua, value: LuaValue| {
                hint_seen
                    .lock()
                    .unwrap()
                    .push(("hint".into(), lua_to_json(lua, &value).unwrap()));
                Ok(())
            })
            .unwrap(),
        )
        .unwrap();
        ui.set(
            "flash",
            lua.create_function(move |_lua, value: LuaValue| {
                let recorded = match value {
                    LuaValue::Nil => Value::Null,
                    LuaValue::String(s) => Value::String(s.to_string_lossy()),
                    other => Value::String(other.to_string().unwrap_or_default()),
                };
                flash_seen.lock().unwrap().push(("flash".into(), recorded));
                Ok(())
            })
            .unwrap(),
        )
        .unwrap();
        maki.set("ui", ui).unwrap();
        lua.globals().set("maki", maki).unwrap();

        forward_host_ui(&lua, "maki.ui.set_status_hint", &[json!([["q", "quit"]])]).unwrap();
        forward_host_ui(&lua, "maki.ui.set_status_hint", &[]).unwrap();
        forward_host_ui(&lua, "maki.ui.flash", &[json!("flash!")]).unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0], ("hint".into(), json!([["q", "quit"]])));
        assert_eq!(seen[1], ("hint".into(), Value::Null));
        assert_eq!(seen[2], ("flash".into(), json!("flash!")));
    }

    #[test]
    fn forwarded_ui_call_without_host_function_errors() {
        let lua = Lua::new();
        let maki = lua.create_table().unwrap();
        maki.set("ui", lua.create_table().unwrap()).unwrap();
        lua.globals().set("maki", maki).unwrap();
        let err = forward_host_ui(&lua, "maki.ui.set_status_hint", &[json!(1)]).unwrap_err();
        assert!(err.contains("set_status_hint"), "{err}");
    }

    #[test]
    fn non_ui_name_is_not_a_host_ui_call() {
        let lua = Lua::new();
        let err = forward_host_ui(&lua, "webfetch", &[]).unwrap_err();
        assert!(err.contains(HOST_UI_PREFIX), "{err}");
    }
}
