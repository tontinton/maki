use mlua::{Function, Lua, MultiValue, Result as LuaResult, Table, Value};

use crate::runtime::enqueue_async_task;

const AWAIT_MIN_ARGS: usize = 2;

pub(crate) fn create_async_table(lua: &Lua) -> LuaResult<Table> {
    let tbl = lua.create_table()?;

    tbl.set(
        "run",
        lua.create_function(|lua, (work_fn, on_finish): (Function, Option<Function>)| {
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
                .call::<Function>((work_fn, cb))?
            } else {
                work_fn
            };
            let work_key = lua.create_registry_value(actual_work)?;
            enqueue_async_task(lua, work_key)?;
            Ok(())
        })?,
    )?;

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
    use super::*;
    use mlua::Lua;
    use test_case::test_case;

    const ERR_TOO_FEW_ARGS: &str = "maki.async.await requires at least 2 arguments: argc, fun, ...";
    const ERR_ARGC_GE_1: &str = "argc must be >= 1";
    const ERR_ARGC_INTEGER: &str = "argc must be an integer";
    const ERR_SECOND_ARG_FN: &str = "second argument must be a function";

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
}
