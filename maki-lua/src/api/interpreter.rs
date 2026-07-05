use std::sync::Arc;
use std::time::Duration;

use maki_agent::tools::interpreter_bridge::{build_async_resolver, build_tool_fns};
use maki_agent::tools::{Deadline, interpreter_ctx};
use maki_agent::{AgentEvent, SharedBuf};
use maki_interpreter::runner;
use mlua::{Function, Lua, Result as LuaResult, Table};

use crate::api::ui::buf::BufHandle;
use crate::api::util::ctx::AgentContext;
use crate::plugin_permissions::{Permission, PluginPermissions};

const PREAMBLE: &str = "import re\nimport asyncio\nimport sys\nimport os\nimport json\n";

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

async fn interpreter_run(
    lua: Lua,
    (code, opts): (String, Table),
) -> LuaResult<(Table, Option<String>)> {
    let timeout_secs: u64 = required(&opts, "timeout")?;
    let max_memory_mb: usize = required(&opts, "max_memory_mb")?;
    let on_output: Function = required(&opts, "on_output")?;
    let agent_ctx: AgentContext = required::<mlua::AnyUserData>(&opts, "agent_ctx")?.take()?;
    let buf: Arc<SharedBuf> = required::<mlua::AnyUserData>(&opts, "buf")?
        .borrow::<BufHandle>()
        .map(|h| Arc::clone(&h.buf))?;

    let timeout = Duration::from_secs(timeout_secs);
    let limits = runner::limits(timeout, max_memory_mb * 1024 * 1024);

    if let Some(id) = &agent_ctx.tool_use_id {
        let _ = agent_ctx.event_tx.send(AgentEvent::LiveToolBuf {
            id: id.clone(),
            body: Arc::clone(&buf),
        });
    }

    let mut ctx = interpreter_ctx(
        &agent_ctx.mode,
        &agent_ctx.event_tx,
        agent_ctx.cancel.clone(),
        Arc::clone(&agent_ctx.permissions),
        Arc::clone(&agent_ctx.file_tracker),
        agent_ctx.user_response_rx.clone(),
        Arc::clone(maki_agent::tools::ToolRegistry::native_arc()),
    );
    ctx.deadline = Deadline::after(timeout);
    ctx.config = agent_ctx.config.clone();

    let (tx, rx) = flume::unbounded::<String>();
    let run = smol::unblock(move || {
        let tools = build_tool_fns(&ctx);
        let resolver = build_async_resolver(&ctx);
        let full_code = format!("{PREAMBLE}{code}");

        let mut flushed = 0usize;
        let result =
            runner::run_streaming(&full_code, &tools, Some(&resolver), limits, &mut |chunk| {
                flushed += chunk.len();
                for line in chunk.lines() {
                    let _ = tx.send(line.to_owned());
                }
            })
            .map_err(|e| e.to_string());
        if let Ok(ir) = &result {
            for line in ir.stdout[flushed..].lines() {
                let _ = tx.send(line.to_owned());
            }
        }
        result
    });
    let recv_loop = async {
        while let Ok(line) = rx.recv_async().await {
            on_output.call::<()>(line)?;
        }
        Ok::<(), mlua::Error>(())
    };
    let (result, cb) = agent_ctx
        .cancel
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
