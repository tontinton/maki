//! Bidirectional JSON <-> `MontyObject` conversion.
//! Lossy corners: NaN floats become `null` (JSON can't represent NaN),
//! BigInts that overflow `i64` become strings, tuples and bytes become arrays,
//! and everything else without a JSON shape (sets, class instances, ...) becomes
//! its Python `repr()`.

use monty_types::unstable::{self, MontyNode};
use monty_types::{MontyObject, ObjectRef};
use serde_json::{Map, Number, Value};

const NONE_TYPE: &str = "NoneType";
const BOOL_TYPE: &str = "bool";
const INT_TYPE: &str = "int";
const FLOAT_TYPE: &str = "float";
const STR_TYPE: &str = "str";
const LIST_TYPE: &str = "list";
const TUPLE_TYPE: &str = "tuple";
const DICT_TYPE: &str = "dict";
const BYTES_TYPE: &str = "bytes";
const REPR_TYPE: &str = "repr";

pub fn json_to_monty(value: Value) -> MontyObject {
    match value {
        Value::Null => MontyObject::none(),
        Value::Bool(b) => MontyObject::bool(b),
        Value::Number(n) => n.as_i64().map_or_else(
            || MontyObject::float(n.as_f64().unwrap_or(0.0)),
            MontyObject::int,
        ),
        Value::String(s) => MontyObject::string(s),
        Value::Array(arr) => MontyObject::list(arr.into_iter().map(json_to_monty)),
        Value::Object(map) => MontyObject::dict(
            map.into_iter()
                .map(|(k, v)| (MontyObject::string(k), json_to_monty(v))),
        ),
    }
}

pub fn monty_to_json(obj: ObjectRef<'_>) -> Value {
    match obj.type_name() {
        NONE_TYPE => Some(Value::Null),
        BOOL_TYPE => obj.as_bool().map(Value::Bool),
        INT_TYPE => Some(obj.as_int().map_or_else(
            || Value::String(obj.to_string()),
            |i| Value::Number(i.into()),
        )),
        FLOAT_TYPE => obj
            .as_float()
            .map(|f| Number::from_f64(f).map_or(Value::Null, Value::Number)),
        STR_TYPE => obj.as_str().map(|s| Value::String(s.to_owned())),
        LIST_TYPE | TUPLE_TYPE => obj
            .items()
            .map(|items| Value::Array(items.into_iter().map(monty_to_json).collect())),
        DICT_TYPE => obj.pairs().map(|pairs| {
            Value::Object(
                pairs
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), monty_to_json(v)))
                    .collect::<Map<_, _>>(),
            )
        }),
        // No stable accessor reads these, and `py_repr` would wrap a `Repr` (the text
        // of a range, an iterator or an unawaited call) in a literal `Repr('...')`.
        BYTES_TYPE | REPR_TYPE => match unstable::node(obj) {
            MontyNode::Bytes(bytes) => Some(Value::Array(
                bytes
                    .iter()
                    .map(|&byte| Value::Number(byte.into()))
                    .collect(),
            )),
            MontyNode::Repr(text) => Some(Value::String(text.clone())),
            _ => None,
        },
        _ => None,
    }
    .unwrap_or_else(|| Value::String(obj.py_repr()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use test_case::test_case;

    #[test_case(json!(null),    MontyObject::none()            ; "null_to_none")]
    #[test_case(json!(true),    MontyObject::bool(true)        ; "bool_true")]
    #[test_case(json!(false),   MontyObject::bool(false)       ; "bool_false")]
    #[test_case(json!(42),      MontyObject::int(42)           ; "integer")]
    #[test_case(json!(-1),      MontyObject::int(-1)           ; "negative_int")]
    #[test_case(json!(2.5),     MontyObject::float(2.5)        ; "float")]
    #[test_case(json!("hello"), MontyObject::string("hello")   ; "string")]
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
        let back = monty_to_json(json_to_monty(input.clone()).as_ref());
        assert_eq!(back, input);
    }

    #[test_case(MontyObject::float(f64::NAN), json!(null)          ; "nan_float_becomes_null")]
    #[test_case(MontyObject::tuple([MontyObject::int(1)]), json!([1]) ; "tuple_becomes_array")]
    #[test_case(MontyObject::dict([(MontyObject::int(1), MontyObject::none())]), json!({"1": null}) ; "non_string_key_uses_str")]
    #[test_case(MontyObject::bytes(*b"ab"), json!([97, 98]) ; "bytes_become_arrays")]
    #[test_case(MontyObject::repr("range(0, 5)"), json!("range(0, 5)") ; "repr_placeholder_becomes_its_text")]
    fn lossy_conversions(obj: MontyObject, expected: Value) {
        assert_eq!(monty_to_json(obj.as_ref()), expected);
    }
}
