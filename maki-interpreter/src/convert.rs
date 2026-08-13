//! Bidirectional JSON <-> `MontyObject` conversion.
//! Lossy corners: NaN floats become `null` (JSON can't represent NaN),
//! BigInts that overflow `i64` become strings, and tuples become arrays.

use monty_types::MontyObject;
use serde_json::Value;

pub fn json_to_monty(value: Value) -> MontyObject {
    match value {
        Value::Null => MontyObject::None,
        Value::Bool(b) => MontyObject::Bool(b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                MontyObject::Int(i)
            } else {
                MontyObject::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => MontyObject::String(s),
        Value::Array(arr) => MontyObject::List(arr.into_iter().map(json_to_monty).collect()),
        Value::Object(map) => MontyObject::Dict(
            map.into_iter()
                .map(|(k, v)| (MontyObject::String(k), json_to_monty(v)))
                .collect::<Vec<_>>()
                .into(),
        ),
    }
}

pub fn monty_to_json(obj: &MontyObject) -> Value {
    match obj {
        MontyObject::None => Value::Null,
        MontyObject::Bool(b) => Value::Bool(*b),
        MontyObject::Int(i) => Value::Number((*i).into()),
        MontyObject::Float(f) => {
            serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number)
        }
        MontyObject::String(s) => Value::String(s.clone()),
        MontyObject::List(items) => Value::Array(items.iter().map(monty_to_json).collect()),
        MontyObject::Tuple(items) => Value::Array(items.iter().map(monty_to_json).collect()),
        MontyObject::Dict(pairs) => {
            let map: serde_json::Map<String, Value> = pairs
                .into_iter()
                .map(|(k, v)| {
                    let key = match k {
                        MontyObject::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    (key, monty_to_json(v))
                })
                .collect();
            Value::Object(map)
        }
        MontyObject::Bytes(b) => {
            Value::Array(b.iter().map(|&byte| Value::Number(byte.into())).collect())
        }
        MontyObject::BigInt(bi) => {
            if let Ok(i) = i64::try_from(bi) {
                Value::Number(i.into())
            } else {
                Value::String(bi.to_string())
            }
        }
        MontyObject::Repr(s) => Value::String(s.clone()),
        _ => Value::String(obj.py_repr()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use test_case::test_case;

    #[test_case(json!(null),    MontyObject::None            ; "null_to_none")]
    #[test_case(json!(true),    MontyObject::Bool(true)      ; "bool_true")]
    #[test_case(json!(false),   MontyObject::Bool(false)     ; "bool_false")]
    #[test_case(json!(42),      MontyObject::Int(42)         ; "integer")]
    #[test_case(json!(-1),      MontyObject::Int(-1)         ; "negative_int")]
    #[test_case(json!(2.5),     MontyObject::Float(2.5)      ; "float")]
    #[test_case(json!("hello"), MontyObject::String("hello".into()) ; "string")]
    fn json_to_monty_scalars(input: Value, expected: MontyObject) {
        assert_eq!(json_to_monty(input), expected);
    }

    #[test_case(json!(null)                       ; "null")]
    #[test_case(json!(true)                       ; "bool")]
    #[test_case(json!(42)                         ; "int")]
    #[test_case(json!(2.5)                        ; "float")]
    #[test_case(json!("text")                     ; "string")]
    #[test_case(json!([1, 2, 3])                  ; "array")]
    #[test_case(json!({"a": 1, "b": [true, null]}); "nested_object")]
    #[test_case(json!([])                         ; "empty_array")]
    #[test_case(json!({})                         ; "empty_object")]
    #[test_case(json!({"key": [1, "two", null]})  ; "mixed_nested")]
    #[test_case(json!(i64::MAX)                   ; "large_i64")]
    fn roundtrip_preserves_value(input: Value) {
        let back = monty_to_json(&json_to_monty(input.clone()));
        assert_eq!(back, input);
    }

    #[test]
    fn nan_float_becomes_null() {
        let obj = MontyObject::Float(f64::NAN);
        assert_eq!(monty_to_json(&obj), Value::Null);
    }
}
