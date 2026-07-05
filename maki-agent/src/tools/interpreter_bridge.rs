use std::collections::HashMap;

use maki_interpreter::runner::ToolFn;
use maki_interpreter::{AsyncResolver, PendingCall};
use serde_json::Value;
use smol::future::block_on;

use crate::agent::tool_dispatch::{self, Emit};
use crate::task_set::TaskSet;

use super::{ToolAudience, ToolContext};

async fn call(
    ctx: &ToolContext,
    name: &str,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<Value, String> {
    ctx.deadline.check()?;
    let input = build_tool_input(args, kwargs)?;
    let done = tool_dispatch::run(
        &ctx.registry,
        ctx.mcp.as_ref(),
        String::new(),
        name,
        &input,
        ctx,
        Emit::Silent,
    )
    .await;
    if done.is_error {
        Err(done.output.as_text())
    } else {
        Ok(Value::String(done.output.as_text()))
    }
}

pub fn build_tool_fns(ctx: &ToolContext) -> HashMap<String, ToolFn> {
    ctx.registry
        .iter()
        .iter()
        .filter(|entry| entry.tool.audience().contains(ToolAudience::INTERPRETER))
        .filter(|entry| super::is_tool_enabled(&ctx.config, entry.name()))
        .map(|entry| {
            let ctx = ctx.clone();
            let f: ToolFn = Box::new(
                move |fn_name: &str, args: Vec<Value>, kwargs: Vec<(String, Value)>| {
                    block_on(call(&ctx, fn_name, &args, &kwargs))
                },
            );
            (entry.name().to_string(), f)
        })
        .collect()
}

pub fn build_async_resolver(ctx: &ToolContext) -> AsyncResolver {
    let ctx = ctx.clone();
    Box::new(move |pending_calls: Vec<PendingCall>| {
        block_on(async {
            let call_ids: Vec<u32> = pending_calls.iter().map(|pc| pc.call_id).collect();
            let mut set = TaskSet::new();
            for pc in pending_calls {
                let ctx = ctx.clone();
                set.spawn(
                    async move { (pc.call_id, call(&ctx, &pc.name, &pc.args, &pc.kwargs).await) },
                );
            }

            let results: Vec<_> = set
                .join_all()
                .await
                .into_iter()
                .zip(&call_ids)
                .map(|(r, &call_id)| {
                    r.unwrap_or_else(|msg| {
                        tracing::error!(error = %msg, "code_execution inner tool panicked");
                        (call_id, Err(format!("tool panicked: {msg}")))
                    })
                })
                .collect();

            Ok(results)
        })
    })
}

pub fn build_tool_input(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    if let Some(first) = args.first()
        && first.is_object()
    {
        return Ok(first.clone());
    }

    if !kwargs.is_empty() {
        let mut obj = serde_json::Map::new();
        for (k, v) in kwargs {
            obj.insert(k.clone(), v.clone());
        }
        return Ok(Value::Object(obj));
    }

    if args.is_empty() {
        return Ok(serde_json::json!({}));
    }

    Err("pass arguments as keyword arguments (e.g. read(path='/file'))".into())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const EXPECTED_ERR: &str = "pass arguments as keyword arguments (e.g. read(path='/file'))";

    #[test_case(&[], &[("path".into(), json!("/foo"))],                              json!({"path": "/foo"})          ; "kwargs")]
    #[test_case(&[json!({"path": "/foo"})], &[],                                     json!({"path": "/foo"})          ; "dict_passthrough")]
    #[test_case(&[], &[],                                                            json!({})                        ; "no_args")]
    #[test_case(&[json!({"a": 1}), json!({"b": 2})], &[],                           json!({"a": 1})                  ; "first_object_ignores_rest")]
    #[test_case(&[json!({"a": 1})], &[("b".into(), json!(2))],                      json!({"a": 1})                  ; "first_object_ignores_kwargs")]
    #[test_case(&[], &[("a".into(), json!(1)), ("b".into(), json!(2))],              json!({"a": 1, "b": 2})         ; "multiple_kwargs_all_included")]
    fn build_tool_input_cases(args: &[Value], kwargs: &[(String, Value)], expected: Value) {
        assert_eq!(build_tool_input(args, kwargs).unwrap(), expected);
    }

    #[test_case(&[json!("hello")], &[]          ; "positional_string")]
    #[test_case(&[json!(1), json!(2)], &[]      ; "multiple_positional_non_objects")]
    fn build_tool_input_rejects_positional_non_objects(args: &[Value], kwargs: &[(String, Value)]) {
        assert_eq!(build_tool_input(args, kwargs).unwrap_err(), EXPECTED_ERR);
    }
}
