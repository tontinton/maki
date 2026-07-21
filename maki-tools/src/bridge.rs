//! Parent-side bridge between the plugin host and the sandbox child.
//!
//! Trusted tool calls the child forwards during a run are answered by
//! invoking the corresponding Lua tool functions from the plugin host.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use maki_agent::agent::UNKNOWN_TOOL_PREFIX;
use maki_agent::tools::interpreter_bridge::build_tool_input;
use maki_interpreter::runner::InterpreterResult;
use maki_sandbox::Sandbox;
use mlua::{Function, Lua, Value as LuaValue};

async fn call_lua_tool(
    lua: Lua,
    func: Function,
    name: String,
    arg: LuaValue,
) -> Result<String, String> {
    let thread = lua
        .create_thread(func)
        .map_err(|e| format!("{name}: {e}"))?;
    let values: mlua::MultiValue = thread
        .into_async(arg)
        .map_err(|e| format!("{name}: {e}"))?
        .await
        .map_err(|e| format!("{name}: {e}"))?;
    maki_lua::lua_tool_result(values).map_err(|e| format!("{name}: {e}"))
}

/// Run `code` inside the sandbox child, exposing the given Lua functions as
/// trusted tools.
pub async fn run_sandbox_with(
    sandbox: &Arc<Sandbox>,
    lua: Lua,
    code: String,
    timeout: Duration,
    fns: HashMap<String, Function>,
    config_json: String,
) -> Result<Result<InterpreterResult, String>, mlua::Error> {
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
                let Some(func) = fns.get(name).cloned() else {
                    return Err(format!("{UNKNOWN_TOOL_PREFIX}: {name}"));
                };
                let input = build_tool_input(&args, &kwargs).map_err(|e| e.to_string())?;
                let arg = maki_lua::json_to_lua(&lua, &input).map_err(|e| e.to_string())?;
                futures_lite::future::block_on(call_lua_tool(
                    lua.clone(),
                    func,
                    name.to_owned(),
                    arg,
                ))
            },
        )
    })
    .await
    .map_err(|e| mlua::Error::runtime(format!("sandbox run: {e}")))?;

    if let Some(err) = result.error {
        return Ok(Err(err));
    }

    Ok(Ok(InterpreterResult {
        output: result.output,
        stdout: result.stdout,
    }))
}
