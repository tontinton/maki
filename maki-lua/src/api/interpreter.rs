//! Runs Python in the monty sandbox with Lua fns as tools. Monty blocks on
//! a `smol::unblock` thread. Stdout and tool-call batches share one FIFO
//! channel so ordering is preserved and cancellation (dropped channel) makes
//! the blocked thread unwind instead of leaking.

use std::collections::HashMap;
use std::time::Duration;

use futures::future::join_all;
use maki_agent::cancel::CancelToken;
use maki_agent::tools::interpreter_bridge::build_tool_input;
use maki_interpreter::error::InterpreterError;
use maki_interpreter::runner::{self, ToolFn};
use maki_interpreter::{AsyncResolver, PendingCall};
use mlua::{Function, Lua, Result as LuaResult, Table};
use serde_json::Value;

use crate::api::util::convert::{json_to_lua, lua_tool_result};
use crate::plugin_permissions::{Permission, PluginPermissions};
use crate::runtime::{TaskHandle, lock_cell};

const BRIDGE_CLOSED: &str = "tool bridge closed (cancelled)";

type CallResults = Vec<(u32, Result<Value, String>)>;

enum BridgeMsg {
    Line(String),
    Calls(Vec<PendingCall>, flume::Sender<CallResults>),
}

pub(crate) fn create_interpreter_table(lua: &Lua, perms: &PluginPermissions) -> LuaResult<Table> {
    let t = lua.create_table()?;
    t.set(
        "run",
        perms.guard_async(Permission::Run, lua, interpreter_run)?,
    )?;
    Ok(t)
}

fn required<T: mlua::FromLua>(opts: &Table, key: &str) -> LuaResult<T> {
    opts.get::<Option<T>>(key)?
        .ok_or_else(|| mlua::Error::runtime(format!("interpreter.run: '{key}' is required")))
}

fn forward_calls(
    tx: &flume::Sender<BridgeMsg>,
    calls: Vec<PendingCall>,
) -> Result<CallResults, InterpreterError> {
    let (reply_tx, reply_rx) = flume::bounded(1);
    tx.send(BridgeMsg::Calls(calls, reply_tx))
        .map_err(|_| InterpreterError::Runtime(BRIDGE_CLOSED.into()))?;
    reply_rx
        .recv()
        .map_err(|_| InterpreterError::Runtime(BRIDGE_CLOSED.into()))
}

async fn call_lua_tool(lua: Lua, f: Option<Function>, pc: &PendingCall) -> Result<Value, String> {
    let Some(f) = f else {
        return Err(format!("unknown tool: {}", pc.name));
    };
    let input = build_tool_input(&pc.args, &pc.kwargs)?;
    let arg = json_to_lua(&lua, &input).map_err(|e| e.to_string())?;
    let values = f
        .call_async::<mlua::MultiValue>(arg)
        .await
        .map_err(|e| e.to_string())?;
    lua_tool_result(values)
        .map(Value::String)
        .map_err(|e| format!("{}: {e}", pc.name))
}

async fn interpreter_run(
    lua: Lua,
    (code, opts): (String, Table),
) -> LuaResult<(Table, Option<String>)> {
    let timeout_secs: u64 = required(&opts, "timeout")?;
    let max_memory_mb: usize = required(&opts, "max_memory_mb")?;
    let on_output: Function = required(&opts, "on_output")?;
    let tools_tbl: Option<Table> = opts.get("tools")?;

    let mut fns: HashMap<String, Function> = HashMap::new();
    if let Some(t) = tools_tbl {
        for pair in t.pairs::<String, Function>() {
            let (name, f) = pair?;
            fns.insert(name, f);
        }
    }
    let names: Vec<String> = fns.keys().cloned().collect();

    let cancel = lua
        .app_data_ref::<TaskHandle>()
        .map(|h| lock_cell(&h).cancel.clone())
        .unwrap_or_else(CancelToken::none);

    let timeout = Duration::from_secs(timeout_secs);
    let limits = runner::limits(timeout, max_memory_mb * 1024 * 1024);

    let (tx, rx) = flume::unbounded::<BridgeMsg>();
    let run = smol::unblock(move || {
        let tools: HashMap<String, ToolFn> = names
            .into_iter()
            .map(|name| {
                let tx = tx.clone();
                let f: ToolFn = Box::new(
                    move |fn_name: &str, args: Vec<Value>, kwargs: Vec<(String, Value)>| {
                        let call = PendingCall {
                            call_id: 0,
                            name: fn_name.to_owned(),
                            args,
                            kwargs,
                        };
                        forward_calls(&tx, vec![call])
                            .map_err(|e| e.to_string())?
                            .pop()
                            .map(|(_, r)| r)
                            .unwrap_or_else(|| Err(BRIDGE_CLOSED.into()))
                    },
                );
                (name, f)
            })
            .collect();
        let resolver: AsyncResolver = {
            let tx = tx.clone();
            Box::new(move |pending| forward_calls(&tx, pending))
        };

        let mut flushed = 0usize;
        let result = runner::run_streaming(&code, &tools, Some(&resolver), limits, &mut |chunk| {
            flushed += chunk.len();
            for line in chunk.lines() {
                let _ = tx.send(BridgeMsg::Line(line.to_owned()));
            }
        })
        .map_err(|e| e.to_string());
        if let Ok(ir) = &result {
            for line in ir.stdout[flushed..].lines() {
                let _ = tx.send(BridgeMsg::Line(line.to_owned()));
            }
        }
        result
    });

    let recv_loop = async {
        while let Ok(msg) = rx.recv_async().await {
            match msg {
                BridgeMsg::Line(line) => on_output.call::<()>(line)?,
                BridgeMsg::Calls(batch, reply) => {
                    let futs = batch.into_iter().map(|pc| {
                        let f = fns.get(&pc.name).cloned();
                        let lua = lua.clone();
                        async move { (pc.call_id, call_lua_tool(lua, f, &pc).await) }
                    });
                    let _ = reply.send(join_all(futs).await);
                }
            }
        }
        Ok::<(), mlua::Error>(())
    };

    let (result, cb) = cancel
        .race(futures_lite::future::zip(run, recv_loop))
        .await
        .map_err(mlua::Error::runtime)?;
    cb?;

    let tbl = lua.create_table()?;
    match result {
        Ok(ir) => {
            if !ir.stdout.is_empty() {
                tbl.set("stdout", ir.stdout.trim_end())?;
            }
            if let Some(val) = ir.output {
                tbl.set("output", val.to_string())?;
            }
            Ok((tbl, None))
        }
        Err(e) => Ok((tbl, Some(e))),
    }
}
