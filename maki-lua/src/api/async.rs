use std::sync::Arc;
use std::time::Duration;

use async_lock::{Semaphore, SemaphoreGuardArc};
use futures::future::join_all;
use maki_agent::cancel::CancelToken;
use maki_lua_macro::{lua_class, lua_fn, lua_table};
use mlua::{Function, Lua, MultiValue, Result as LuaResult, Table, Value};

use crate::docs::{FnDoc, ParamDoc};
use crate::runtime::{TaskHandle, enqueue_async_task, lock_cell, register_cancel_hook};

const AWAIT_MIN_ARGS: usize = 2;
const PERMIT_RELEASED_ERR: &str = "permit already released";
const SLEEP_NEGATIVE_ERR: &str = "maki.async.sleep: ms must be >= 0";

/// Cancel-aware counting semaphore. Permits release on `:release()` or gc.
struct LuaSemaphore {
    sem: Arc<Semaphore>,
}

struct LuaPermit {
    guard: std::sync::Mutex<Option<SemaphoreGuardArc>>,
}

/// Wait for a permit from the semaphore. Your coroutine suspends until a slot
/// opens up. If the owning task is cancelled, the acquire is cancelled too.
///
/// @return (maki.async.Permit) A permit handle. Call `:release()` when done, or let it be garbage collected.
/// @example
/// local sem = maki.async.semaphore(3)
/// local permit = sem:acquire()
/// -- do work that needs the slot
/// permit:release()
#[lua_fn]
async fn acquire(lua: Lua, this: mlua::UserDataRef<LuaSemaphore>) -> LuaResult<LuaPermit> {
    let sem = Arc::clone(&this.sem);
    drop(this);
    let cancel = lua
        .app_data_ref::<TaskHandle>()
        .map(|h| lock_cell(&h).cancel.clone())
        .unwrap_or_else(CancelToken::none);
    let guard = cancel
        .race(sem.acquire_arc())
        .await
        .map_err(mlua::Error::runtime)?;
    Ok(LuaPermit {
        guard: std::sync::Mutex::new(Some(guard)),
    })
}

lua_class! {
    /// A counting semaphore for limiting how many tasks run at once.
    ///
    /// Create one with `maki.async.semaphore(n)`, then call `:acquire()` to
    /// get a permit before doing work. If the task is cancelled, the acquire
    /// is cancelled too.
    "maki.async.Semaphore" => LuaSemaphore, SEMAPHORE_DOCS [acquire]
}

/// Give the permit back to the semaphore so another task can acquire it.
/// Throws if you already released this permit.
#[lua_fn]
fn release(_lua: &Lua, this: &LuaPermit) -> LuaResult<()> {
    let released = this
        .guard
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .is_some();
    if !released {
        return Err(mlua::Error::runtime(PERMIT_RELEASED_ERR));
    }
    Ok(())
}

lua_class! {
    /// One slot in a semaphore, obtained from `Semaphore:acquire()`.
    ///
    /// The slot is held until you call `:release()` or until the permit is
    /// garbage collected. Releasing early lets other tasks acquire sooner.
    "maki.async.Permit" => LuaPermit, PERMIT_DOCS [release]
}

/// Fire off a function as a new async task. It runs in the background and
/// you do not wait for it. If you need the result, pass an {on_finish}
/// callback.
///
/// The task must finish within 60 seconds; waiting minutes for a build or a
/// subagent inside one dies partway through.
///
/// @param fn function Zero-argument function to execute.
/// @param on_finish function? Optional callback `function(err, result)`. Called once {fn} completes.
/// @example
/// maki.async.run(function()
///   local data = expensive_fetch()
///   process(data)
/// end)
#[lua_fn]
fn run(lua: &Lua, r#fn: Function, on_finish: Option<Function>) -> LuaResult<()> {
    let actual_work = if let Some(cb) = on_finish {
        lua.load(
            r#"
                local work, finish = ...
                return function()
                    local ok, result = pcall(work)
                    if ok then
                        finish(nil, result)
                    else
                        finish(result)
                    end
                end
            "#,
        )
        .call::<Function>((r#fn, cb))?
    } else {
        r#fn
    };
    let work_key = lua.create_registry_value(actual_work)?;
    enqueue_async_task(lua, work_key)?;
    Ok(())
}

/// Register {fn} to run as soon as the current task is cancelled or hits
/// its deadline, without waiting for whatever it is doing to finish. Use
/// it to paint the cancelled state: a handler waiting on children
/// (`gather`, `call_tool`) stays parked until they wind down, so anything
/// after the wait is too late to reach the screen.
///
/// The callback receives the reason (`"cancelled"` or `"timeout"`) and may
/// still call `ctx:finish`; the host prefers that reply over the generic
/// cancelled/timeout error. Mark it `is_error = true` and end it with a
/// marker, so the model knows the output it gets is cut short.
///
/// The callback runs outside your coroutine, so it must not yield. It
/// fires at most once, immediately if the task is already cancelled. An
/// error inside it is logged and never reaches your handler, and the
/// other hooks still run.
///
/// @param fn function Function to run on cancel; receives the reason string.
/// @example
/// maki.async.on_cancel(function(reason)
///   view:append({ { reason, "tool_error" } })
///   ctx:finish({ llm_output = partial .. "\n[cancelled; output is partial]", is_error = true })
/// end)
/// maki.async.gather(children)
#[lua_fn]
fn on_cancel(lua: &Lua, r#fn: Function) -> LuaResult<()> {
    register_cancel_hook(lua, r#fn)
}

/// Run all functions in {fns} at the same time and collect their results.
/// Unlike `join`, this gives you back the return value (or error) from each
/// function. The results are in the same order as the input.
///
/// Each entry in the result array has `ok` (boolean), and either `value`
/// (on success) or `err` (string, on failure).
///
/// @param fns table Array of zero-argument functions.
/// @return (table) Array of result tables, one per function.
/// @example
/// local results = maki.async.gather({
///   function() return fetch("a.txt") end,
///   function() return fetch("b.txt") end,
/// })
/// for i, r in ipairs(results) do
///   if r.ok then print(r.value) else print("error: " .. r.err) end
/// end
#[lua_fn]
async fn gather(lua: Lua, fns: Table) -> LuaResult<Table> {
    let count = fns.raw_len();
    let mut children = Vec::with_capacity(count);
    for i in 1..=count {
        let f: Function = fns
            .raw_get(i)
            .map_err(|_| mlua::Error::runtime(format!("gather: funs[{i}] must be a function")))?;
        children.push(lua.create_thread(f)?);
    }
    let results = join_all(
        children
            .into_iter()
            .map(|thread| async move { thread.into_async::<Value>(())?.await }),
    )
    .await;
    let out = lua.create_table_with_capacity(count, 0)?;
    for (i, res) in results.into_iter().enumerate() {
        let entry = lua.create_table()?;
        match res {
            Ok(value) => {
                entry.set("ok", true)?;
                entry.set("value", value)?;
            }
            Err(e) => {
                entry.set("ok", false)?;
                entry.set("err", e.to_string())?;
            }
        }
        out.raw_set(i + 1, entry)?;
    }
    Ok(out)
}

/// Create a counting semaphore that allows at most {n} concurrent permits.
/// Use this to limit how many tasks hit a resource at the same time.
///
/// @param n integer Maximum number of concurrent permits. Values below 1 are clamped to 1.
/// @return (maki.async.Semaphore) A new semaphore.
/// @example
/// local sem = maki.async.semaphore(5)
/// -- each task acquires a permit before doing work
/// local permit = sem:acquire()
/// do_work()
/// permit:release()
#[lua_fn]
fn semaphore(_lua: &Lua, n: usize) -> LuaResult<LuaSemaphore> {
    Ok(LuaSemaphore {
        sem: Arc::new(Semaphore::new(n.max(1))),
    })
}

/// Suspend the current coroutine for {ms} milliseconds. The timer runs on
/// the async executor, so nothing spins and other tasks keep running.
/// Cancelling the owning task interrupts the sleep with the cancel error.
///
/// For a timer that has to outlive the tool call that started it, such
/// as a toast dismissing itself, use `maki.defer_fn`.
///
/// @param ms integer Milliseconds to wait. Must be >= 0.
/// @example
/// maki.async.run(function()
///   maki.async.sleep(250)
///   retry()
/// end)
#[lua_fn]
async fn sleep(lua: Lua, ms: i64) -> LuaResult<()> {
    if ms < 0 {
        return Err(mlua::Error::runtime(SLEEP_NEGATIVE_ERR));
    }
    let cancel = lua
        .app_data_ref::<TaskHandle>()
        .map(|h| lock_cell(&h).cancel.clone())
        .unwrap_or_else(CancelToken::none);
    cancel
        .race(smol::Timer::after(Duration::from_millis(ms as u64)))
        .await
        .map_err(mlua::Error::runtime)?;
    Ok(())
}

/// `await`, `wrap`, and `join` are registered by hand below: `await`
/// consumes a raw `MultiValue` and the other two are Lua chunks closing over
/// the table.
#[allow(non_upper_case_globals)]
const await__doc: FnDoc = FnDoc {
    name: "await",
    args: "{argc}, {fn}, {...}",
    desc: "Turn a callback-based function into a normal call you can use in a coroutine. It calls `fn(..., callback)`, inserting the callback at position {argc}, then suspends your coroutine until the callback fires. You get back whatever the callback was called with.",
    params: &[
        ParamDoc {
            name: "{argc}",
            ty: "integer",
            desc: "Total number of positional arguments {fn} expects (including the callback). Must be >= 1.",
        },
        ParamDoc {
            name: "{fn}",
            ty: "function",
            desc: "Callback-based function to call.",
        },
        ParamDoc {
            name: "{...}",
            ty: "any",
            desc: "Extra arguments forwarded to {fn} before the injected callback.",
        },
    ],
    returns: "(...) Values passed by the caller to the injected callback.",
    guard: None,
    example: "local result = maki.async.await(2, http.get, url)",
};

#[allow(non_upper_case_globals)]
const wrap__doc: FnDoc = FnDoc {
    name: "wrap",
    args: "{argc}, {fn}",
    desc: "Create a coroutine-friendly wrapper around a callback-based function. The wrapper calls `maki.async.await` for you, so you can use the result like a normal function call.",
    params: &[
        ParamDoc {
            name: "{argc}",
            ty: "integer",
            desc: "Callback position, forwarded to `maki.async.await`.",
        },
        ParamDoc {
            name: "{fn}",
            ty: "function",
            desc: "Callback-based function to wrap.",
        },
    ],
    returns: "(function) Wrapped function you can call like a normal function.",
    guard: None,
    example: "local get = maki.async.wrap(2, http.get)\nlocal body = get(url)",
};

#[allow(non_upper_case_globals)]
const join__doc: FnDoc = FnDoc {
    name: "join",
    args: "{max_jobs}, {fns}",
    desc: "Run all functions in {fns} with at most {max_jobs} going at once. Waits until every function has finished. Unlike `gather`, this does not return individual results.",
    params: &[
        ParamDoc {
            name: "{max_jobs}",
            ty: "integer",
            desc: "Maximum number of functions running at the same time.",
        },
        ParamDoc {
            name: "{fns}",
            ty: "table",
            desc: "Array of zero-argument functions to execute.",
        },
    ],
    returns: "",
    guard: None,
    example: "maki.async.join(4, {\n  function() process(files[1]) end,\n  function() process(files[2]) end,\n  function() process(files[3]) end,\n})",
};

lua_table! {
    /// Tools for running things concurrently in Lua plugins.
    ///
    /// Use `run` to fire off background tasks, `gather` or `join` to run
    /// several functions at once, and `semaphore` to limit concurrency.
    /// The `await` and `wrap` helpers bridge callback-based APIs into
    /// coroutine-friendly calls.
    ///
    /// ```lua
    /// local results = maki.async.gather({
    ///   function() return fetch("a.txt") end,
    ///   function() return fetch("b.txt") end,
    /// })
    /// ```
    extend "maki.async" => pub(crate) fn add_async_fns(), DOCS [
        run, sleep, manual r#await, manual wrap, manual join, gather, semaphore, on_cancel,
    ]
}

pub(crate) fn create_async_table(lua: &Lua) -> LuaResult<Table> {
    let tbl = lua.create_table()?;
    add_async_fns(&tbl, lua)?;

    tbl.set(
        "await",
        lua.create_async_function(|lua, args: MultiValue| async move {
            let mut args_vec: Vec<Value> = args.into_vec();
            if args_vec.len() < AWAIT_MIN_ARGS {
                return Err(mlua::Error::runtime(
                    "maki.async.await requires at least 2 arguments: argc, fun, ...",
                ));
            }
            let argc = match &args_vec[0] {
                Value::Integer(n) if *n >= 1 => *n as usize,
                Value::Integer(_) => {
                    return Err(mlua::Error::runtime("argc must be >= 1"));
                }
                _ => return Err(mlua::Error::runtime("argc must be an integer")),
            };
            args_vec.remove(0);
            let fun = match args_vec.remove(0) {
                Value::Function(f) => f,
                _ => return Err(mlua::Error::runtime("second argument must be a function")),
            };

            let (tx, rx) = flume::bounded(1);

            let callback = lua.create_function(move |_lua, values: MultiValue| {
                tx.send(values).ok();
                Ok(())
            })?;

            let insert_pos = (argc - 1).min(args_vec.len());
            args_vec.insert(insert_pos, Value::Function(callback));

            fun.call::<()>(MultiValue::from_iter(args_vec))?;

            let result = rx
                .recv_async()
                .await
                .map_err(|_| mlua::Error::runtime("async.await: callback was never called"))?;
            Ok(result)
        })?,
    )?;

    tbl.set(
        "join",
        lua.load(
            r#"
            local async_tbl = ...
            return function(max_jobs, funs)
                if #funs == 0 then return end
                max_jobs = math.min(max_jobs, #funs)
                local remaining = {}
                for i = max_jobs + 1, #funs do
                    remaining[#remaining + 1] = funs[i]
                end
                local to_go = #funs
                async_tbl.await(1, function(on_finish)
                    local function run_next()
                        to_go = to_go - 1
                        if to_go == 0 then
                            on_finish()
                        elseif #remaining > 0 then
                            async_tbl.run(table.remove(remaining, 1), run_next)
                        end
                    end
                    for i = 1, max_jobs do
                        async_tbl.run(funs[i], run_next)
                    end
                end)
            end
        "#,
        )
        .call::<Function>(&tbl)?,
    )?;

    tbl.set(
        "wrap",
        lua.load(
            r#"
            local async_tbl = ...
            return function(argc, fun)
                return function(...)
                    return async_tbl.await(argc, fun, ...)
                end
            end
        "#,
        )
        .call::<Function>(&tbl)?,
    )?;

    Ok(tbl)
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::sync::Mutex;

    use futures_lite::future::{or, poll_once};
    use maki_agent::cancel::CancelTrigger;
    use mlua::Lua;
    use test_case::test_case;

    use super::*;
    use crate::runtime::{CANCELLED_MSG, TaskCell, TaskScope, block_on_or_fail};

    const ERR_TOO_FEW_ARGS: &str = "maki.async.await requires at least 2 arguments: argc, fun, ...";
    const ERR_ARGC_GE_1: &str = "argc must be >= 1";
    const ERR_ARGC_INTEGER: &str = "argc must be an integer";
    const ERR_SECOND_ARG_FN: &str = "second argument must be a function";
    const ERR_SLEEP_NEGATIVE: &str = "ms must be >= 0";

    fn setup() -> (Lua, Table) {
        let lua = Lua::new();
        let tbl = create_async_table(&lua).unwrap();
        lua.globals().set("async_tbl", tbl.clone()).unwrap();
        (lua, tbl)
    }

    #[test_case(r#"return async_tbl.await(1)"#, ERR_TOO_FEW_ARGS ; "too_few_args")]
    #[test_case(r#"return async_tbl.await(0, function() end)"#, ERR_ARGC_GE_1 ; "argc_below_one")]
    #[test_case(r#"return async_tbl.await(nil, function() end)"#, ERR_ARGC_INTEGER ; "argc_non_integer")]
    #[test_case(r#"return async_tbl.await(1, 42)"#, ERR_SECOND_ARG_FN ; "second_arg_not_fn")]
    fn await_validation(code: &str, expected_err: &str) {
        smol::block_on(async {
            let (lua, _tbl) = setup();
            let err = lua.load(code).eval_async::<Value>().await.unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains(expected_err),
                "expected error containing {expected_err:?}, got: {msg}"
            );
        });
    }

    #[test_case(r#"return async_tbl.sleep(-1)"#, ERR_SLEEP_NEGATIVE ; "negative_ms")]
    fn sleep_validation(code: &str, expected_err: &str) {
        smol::block_on(async {
            let (lua, _tbl) = setup();
            let err = lua.load(code).eval_async::<Value>().await.unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains(expected_err),
                "expected error containing {expected_err:?}, got: {msg}"
            );
        });
    }

    #[test]
    fn sleep_suspends_for_the_requested_duration() {
        smol::block_on(async {
            let (lua, _tbl) = setup();
            let began = std::time::Instant::now();
            lua.load("async_tbl.sleep(30)").exec_async().await.unwrap();
            assert!(
                began.elapsed() >= Duration::from_millis(30),
                "sleep(30) returned early after {:?}",
                began.elapsed()
            );
        });
    }

    #[test]
    fn sleep_observes_caller_cancel() {
        smol::block_on(async {
            let (lua, _tbl) = setup();
            lua.set_app_data::<TaskHandle>(cancelled_task_handle());
            let code = r#"
                local ok, err = pcall(async_tbl.sleep, 60_000)
                return ok, tostring(err)
            "#;
            let vals: Vec<Value> = lua
                .load(code)
                .eval_async::<MultiValue>()
                .await
                .unwrap()
                .into_vec();
            assert!(!vals[0].as_boolean().unwrap());
            assert!(
                vals[1]
                    .as_string()
                    .unwrap()
                    .to_string_lossy()
                    .contains(CANCELLED_MSG),
                "sleep should observe the caller's cancel token"
            );
        });
    }

    #[test_case(1, &[], 0 ; "no_extra_args")]
    #[test_case(3, &["a", "b"], 2 ; "with_extra_args")]
    fn await_callback_insertion_position(argc: usize, extra: &[&str], expected_pos: usize) {
        smol::block_on(async {
            let (lua, _tbl) = setup();

            let extra_str = extra
                .iter()
                .map(|s| format!(r#""{s}""#))
                .collect::<Vec<_>>()
                .join(", ");
            let trailing = if extra_str.is_empty() {
                String::new()
            } else {
                format!(", {extra_str}")
            };

            let code = format!(
                r#"
                local pos = -1
                local function target(...)
                    local args = {{...}}
                    for i, v in ipairs(args) do
                        if type(v) == "function" then
                            pos = i - 1
                            v()
                            return
                        end
                    end
                end
                async_tbl.await({argc}, target{trailing})
                return pos
                "#
            );

            let result = lua.load(&code).eval_async::<i64>().await.unwrap();
            assert_eq!(result, expected_pos as i64);
        });
    }

    #[test]
    fn await_returns_multivalue_from_callback() {
        smol::block_on(async {
            let (lua, _tbl) = setup();
            let code = r#"
                local function producer(cb)
                    cb("hello", 42, true)
                end
                return async_tbl.await(1, producer)
            "#;
            let results = lua.load(code).eval_async::<MultiValue>().await.unwrap();
            let vals: Vec<Value> = results.into_vec();
            assert_eq!(vals.len(), 3);
            assert_eq!(vals[0].as_string().unwrap().to_string_lossy(), "hello");
            assert_eq!(vals[1].as_integer().unwrap(), 42);
            assert!(vals[2].as_boolean().unwrap());
        });
    }

    #[test]
    fn wrap_creates_callable_wrapper() {
        smol::block_on(async {
            let (lua, _tbl) = setup();
            let code = r#"
                local function async_add(a, b, cb)
                    cb(a + b)
                end
                local wrapped = async_tbl.wrap(3, async_add)
                return wrapped(10, 32)
            "#;
            let result = lua.load(code).eval_async::<i64>().await.unwrap();
            assert_eq!(result, 42);
        });
    }

    #[test]
    fn gather_preserves_input_order_and_values() {
        smol::block_on(async {
            let (lua, _tbl) = setup();
            let code = r#"
                local r = async_tbl.gather({
                    function() return "a" end,
                    function() error("boom") end,
                    function() return 42 end,
                })
                return r[1].ok, r[1].value, r[2].ok, tostring(r[2].err), r[3].value
            "#;
            let vals: Vec<Value> = lua
                .load(code)
                .eval_async::<MultiValue>()
                .await
                .unwrap()
                .into_vec();
            assert!(vals[0].as_boolean().unwrap());
            assert_eq!(vals[1].as_string().unwrap().to_string_lossy(), "a");
            assert!(!vals[2].as_boolean().unwrap());
            assert!(
                vals[3]
                    .as_string()
                    .unwrap()
                    .to_string_lossy()
                    .contains("boom"),
                "err should contain the child's message"
            );
            assert_eq!(vals[4].as_integer().unwrap(), 42);
        });
    }

    #[test]
    fn gather_rejects_non_function_entries() {
        smol::block_on(async {
            let (lua, _tbl) = setup();
            let msg = lua
                .load(r#"return async_tbl.gather({ function() end, 42 })"#)
                .eval_async::<Value>()
                .await
                .unwrap_err()
                .to_string();
            assert!(msg.contains("funs[2] must be a function"), "got: {msg}");
        });
    }

    #[test]
    fn gather_runs_children_concurrently() {
        smol::block_on(async {
            let (lua, _tbl) = setup();
            // child 1 parks on a held semaphore; child 2 releases it.
            // Sequential execution would deadlock here.
            lua.load("sem = async_tbl.semaphore(1); held = sem:acquire()")
                .exec_async()
                .await
                .unwrap();
            let code = r#"
                local r = async_tbl.gather({
                    function()
                        local p = sem:acquire()
                        p:release()
                        return "waited"
                    end,
                    function()
                        held:release()
                        return "released"
                    end,
                })
                return r[1].value, r[2].value
            "#;
            let vals: Vec<Value> = lua
                .load(code)
                .eval_async::<MultiValue>()
                .await
                .unwrap()
                .into_vec();
            assert_eq!(vals[0].as_string().unwrap().to_string_lossy(), "waited");
            assert_eq!(vals[1].as_string().unwrap().to_string_lossy(), "released");
        });
    }

    #[test]
    fn gather_children_see_caller_cancel() {
        smol::block_on(async {
            let (lua, _tbl) = setup();
            lua.load("sem = async_tbl.semaphore(1); held = sem:acquire()")
                .exec_async()
                .await
                .unwrap();
            lua.set_app_data::<TaskHandle>(cancelled_task_handle());
            let code = r#"
                local r = async_tbl.gather({ function() return sem:acquire() end })
                return r[1].ok, tostring(r[1].err)
            "#;
            let vals: Vec<Value> = lua
                .load(code)
                .eval_async::<MultiValue>()
                .await
                .unwrap()
                .into_vec();
            assert!(!vals[0].as_boolean().unwrap());
            assert!(
                vals[1]
                    .as_string()
                    .unwrap()
                    .to_string_lossy()
                    .contains(CANCELLED_MSG),
                "child should observe caller's cancel token"
            );
        });
    }

    fn cancelled_task_handle() -> TaskHandle {
        let (trigger, token) = CancelToken::new();
        trigger.cancel();
        Arc::new(Mutex::new(TaskCell::new(token, None, None)))
    }

    #[test_case(0 ; "zero_clamps_to_capacity_one")]
    #[test_case(1 ; "capacity_one")]
    fn semaphore_acquire_blocks_at_capacity_until_release(n: usize) {
        smol::block_on(async {
            let (lua, _tbl) = setup();
            lua.load(format!(
                "sem = async_tbl.semaphore({n}); p1 = sem:acquire()"
            ))
            .exec_async()
            .await
            .unwrap();
            let mut second = pin!(lua.load("p2 = sem:acquire()").exec_async());
            assert!(
                poll_once(second.as_mut()).await.is_none(),
                "second acquire must block while first permit is held"
            );
            lua.load("p1:release()").exec().unwrap();
            second.await.unwrap();
            lua.load("assert(p2 ~= nil)").exec().unwrap();
        });
    }

    #[test]
    fn semaphore_double_release_errors() {
        smol::block_on(async {
            let (lua, _tbl) = setup();
            lua.load("local sem = async_tbl.semaphore(1); p = sem:acquire(); p:release()")
                .exec_async()
                .await
                .unwrap();
            let msg = lua.load("p:release()").exec().unwrap_err().to_string();
            assert!(
                msg.contains(PERMIT_RELEASED_ERR),
                "expected error containing {PERMIT_RELEASED_ERR:?}, got: {msg}"
            );
        });
    }

    #[test]
    fn semaphore_gc_of_permit_releases_slot() {
        smol::block_on(async {
            let (lua, _tbl) = setup();
            lua.load("sem = async_tbl.semaphore(1); do local p = sem:acquire() end")
                .exec_async()
                .await
                .unwrap();
            lua.gc_collect().unwrap();
            lua.gc_collect().unwrap();
            let reacquire = pin!(lua.load("return sem:acquire() ~= nil").eval_async::<bool>());
            match poll_once(reacquire).await {
                Some(result) => assert!(result.unwrap()),
                None => panic!("acquire must complete immediately after permit was gc'd"),
            }
        });
    }

    #[test]
    fn semaphore_acquire_errors_when_task_cancelled() {
        smol::block_on(async {
            let (lua, _tbl) = setup();
            lua.load("sem = async_tbl.semaphore(1); held = sem:acquire()")
                .exec_async()
                .await
                .unwrap();
            lua.set_app_data::<TaskHandle>(cancelled_task_handle());
            let msg = lua
                .load("return sem:acquire()")
                .eval_async::<Value>()
                .await
                .unwrap_err()
                .to_string();
            assert!(
                msg.contains(CANCELLED_MSG),
                "expected error containing {CANCELLED_MSG:?}, got: {msg}"
            );
        });
    }

    const HOOK_NEVER_FIRED: &str = "cancel hook never fired";
    const PARKED_CHILD: &str = r#"
        function()
            return async_tbl.await(1, function(cb) parked_cb = cb end)
        end
    "#;
    const RELEASE_PARKED_CHILD: &str = r#"parked_cb("done")"#;
    const CHILD_VALUE: &str = "done";
    const HOOK_RAW_YIELD: &str = "coroutine.yield()";
    const HOOK_AWAIT: &str = "async_tbl.await(1, function() end)";
    const HOOK_LATE_MSG: &str = "the hook must fire while the wait is still parked, not after it";
    const HOOK_SURVIVED_MSG: &str = "a hook that waits from outside its coroutine must fail there";
    const TASK_SURVIVED_MSG: &str = "the task must keep working after a hook blew up";

    fn install_notify(lua: &Lua) -> flume::Receiver<()> {
        let (fired_tx, fired_rx) = flume::bounded(1);
        let notify = lua
            .create_function(move |_, ()| {
                fired_tx.send(()).ok();
                Ok(())
            })
            .unwrap();
        lua.globals().set("notify", notify).unwrap();
        fired_rx
    }

    fn live_scope(lua: &Lua) -> (CancelTrigger, TaskScope) {
        let (trigger, token) = CancelToken::new();
        (
            trigger,
            TaskScope::new(lua, TaskCell::new(token, None, None)),
        )
    }

    /// The composition `plugins/batch` leans on, and the one thing the
    /// runtime's own hook tests cannot show: the handler is parked deep inside
    /// a real `gather` whose child never finishes, so the hook runs on a VM
    /// whose coroutine is suspended. Waiting from there is a plugin bug, raw
    /// or through `maki.async`, and neither may cost the hooks behind it nor
    /// the task's own result.
    #[test_case(HOOK_RAW_YIELD ; "raw_yield")]
    #[test_case(HOOK_AWAIT ; "awaiting")]
    fn on_cancel_hook_fires_while_gather_is_still_parked(bad_hook_body: &str) {
        let (lua, _tbl) = setup();
        let (trigger, scope) = live_scope(&lua);
        let fired_rx = install_notify(&lua);

        let code = format!(
            r#"
            gather_returned = false
            bad_hook_finished = false
            async_tbl.on_cancel(function()
                {bad_hook_body}
                bad_hook_finished = true
            end)
            async_tbl.on_cancel(notify)
            local r = async_tbl.gather({{ {PARKED_CHILD} }})
            gather_returned = true
            return r[1].ok, r[1].value
            "#
        );

        let vals: Vec<Value> = block_on_or_fail(or(
            scope.scope_future(lua.load(&code).eval_async::<MultiValue>()),
            async {
                trigger.cancel();
                fired_rx.recv_async().await.expect(HOOK_NEVER_FIRED);
                assert!(
                    !lua.globals().get::<bool>("gather_returned").unwrap(),
                    "{HOOK_LATE_MSG}"
                );
                assert!(
                    !lua.globals().get::<bool>("bad_hook_finished").unwrap(),
                    "{HOOK_SURVIVED_MSG}"
                );
                lua.load(RELEASE_PARKED_CHILD).exec().unwrap();
                std::future::pending().await
            },
        ))
        .unwrap()
        .into_vec();

        assert!(vals[0].as_boolean().unwrap(), "gather child must succeed");
        assert_eq!(
            vals[1].as_string().unwrap().to_string_lossy(),
            CHILD_VALUE,
            "{TASK_SURVIVED_MSG}"
        );
    }

    /// A handler queued on a full semaphore waits on an `Event` that knows
    /// nothing about the token, so the hook is the only cleanup that can run
    /// before the acquire gives up. The Lua-side flag pins the order: the hook
    /// ran while the acquire was still parked.
    #[test]
    fn on_cancel_hook_fires_while_a_semaphore_acquire_is_still_parked() {
        let (lua, _tbl) = setup();
        let (trigger, scope) = live_scope(&lua);

        let code = r#"
            hook_fired = false
            local sem = async_tbl.semaphore(1)
            held_permit = sem:acquire()
            async_tbl.on_cancel(function() hook_fired = true end)
            local ok, err = pcall(function() return sem:acquire() end)
            return hook_fired, ok, tostring(err)
        "#;

        let vals: Vec<Value> = block_on_or_fail(or(
            scope.scope_future(lua.load(code).eval_async::<MultiValue>()),
            async {
                trigger.cancel();
                std::future::pending().await
            },
        ))
        .unwrap()
        .into_vec();

        assert!(vals[0].as_boolean().unwrap(), "{HOOK_LATE_MSG}");
        assert!(
            !vals[1].as_boolean().unwrap(),
            "a cancelled acquire must not hand out a permit"
        );
        let err = vals[2].as_string().unwrap().to_string_lossy();
        assert!(
            err.contains(CANCELLED_MSG),
            "expected error containing {CANCELLED_MSG:?}, got: {err}"
        );
    }
}
