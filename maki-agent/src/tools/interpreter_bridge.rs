use serde_json::Value;

use crate::agent::tool_dispatch::{self, Emit};

use super::ToolContext;

pub const IMAGE_NOT_VISIBLE_NOTE: &str =
    "image pixels are not visible from here; call the view_image tool directly";

pub async fn dispatch(ctx: &ToolContext, name: &str, input: &Value) -> Result<String, String> {
    ctx.deadline.check()?;
    let done = tool_dispatch::run(
        &ctx.registry,
        ctx.mcp.as_ref(),
        String::new(),
        name,
        input,
        ctx,
        Emit::Silent,
    )
    .await;
    if done.is_error {
        Err(done.output.as_text())
    } else if let crate::ToolOutput::Image { text, .. } = &done.output {
        // The pixels are dropped here; say so instead of implying they were seen.
        Ok(format!("{text} ({IMAGE_NOT_VISIBLE_NOTE})"))
    } else {
        Ok(done.output.as_text())
    }
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
