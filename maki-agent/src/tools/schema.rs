//! Each tool has one `ParamSchema` that drives both the JSON Schema sent to the
//! LLM and the validator that checks its response. If those two ever disagree
//! the model gets a schema that lies about what we accept, so one source of
//! truth keeps us honest.
//!
//! Validation errors are our own types with a single `Display` impl so the
//! model never sees a raw serde message we did not write.

use std::fmt::{self, Display, Formatter, Write};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tracing::warn;

pub(crate) const PARAM_PREVIEW_MAX: usize = 120;

const PREVIEW_SUFFIX: &str = "...";
const JSON_ENCODED_ARRAY_HINT: &str = "Pass a JSON array, not a JSON-encoded string.";
const JSON_ENCODED_OBJECT_HINT: &str = "Pass a JSON object, not a JSON-encoded string.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    Null,
    Bool,
    Integer,
    Number,
    String,
    Array,
    Object,
}

impl ParamKind {
    pub(crate) fn of(v: &Value) -> Self {
        match v {
            Value::Null => Self::Null,
            Value::Bool(_) => Self::Bool,
            Value::Number(n) if n.is_i64() || n.is_u64() => Self::Integer,
            Value::Number(_) => Self::Number,
            Value::String(_) => Self::String,
            Value::Array(_) => Self::Array,
            Value::Object(_) => Self::Object,
        }
    }
}

impl Display for ParamKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Null => "null",
            Self::Bool => "boolean",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::String => "string",
            Self::Array => "array",
            Self::Object => "object",
        })
    }
}

pub(crate) type Property = (&'static str, &'static ParamSchema, bool);

#[derive(Debug)]
pub enum ParamSchema {
    Primitive {
        kind: ParamKind,
        description: &'static str,
    },
    Enum {
        variants: &'static [&'static str],
        description: &'static str,
    },
    Array {
        items: &'static ParamSchema,
        description: &'static str,
    },
    Object {
        properties: &'static [Property],
        description: &'static str,
    },
    Any {
        description: &'static str,
    },
}

pub fn to_json_schema(s: &ParamSchema) -> Value {
    match s {
        ParamSchema::Primitive { kind, description } => {
            json!({ "type": kind.to_string(), "description": description })
        }
        ParamSchema::Enum {
            variants,
            description,
        } => {
            let mut v = json!({ "type": "string", "enum": variants });
            if !description.is_empty() {
                v["description"] = json!(description);
            }
            v
        }
        ParamSchema::Array { items, description } => json!({
            "type": "array",
            "description": description,
            "items": to_json_schema(items),
        }),
        ParamSchema::Object {
            properties,
            description,
        } => {
            let props: serde_json::Map<String, Value> = properties
                .iter()
                .map(|(name, sub, _)| ((*name).into(), to_json_schema(sub)))
                .collect();
            let required: Vec<&&str> = properties
                .iter()
                .filter_map(|(name, _, req)| req.then_some(name))
                .collect();
            let mut v = json!({
                "type": "object",
                "required": required,
                "properties": props,
                "additionalProperties": false,
            });
            if !description.is_empty() {
                v["description"] = json!(description);
            }
            v
        }
        ParamSchema::Any { description } => {
            if description.is_empty() {
                json!({})
            } else {
                json!({ "description": description })
            }
        }
    }
}

/// Leaks everything to get `&'static` lifetimes matching native tool schemas.
/// The leaked set is small and fixed per session, so this is fine.
pub fn try_from_json(v: &Value) -> Result<&'static ParamSchema, String> {
    let description: &'static str = v
        .get("description")
        .and_then(|d| d.as_str())
        .map(|s| -> &'static str { Box::leak(s.to_owned().into_boxed_str()) })
        .unwrap_or("");

    let type_str = v.get("type").and_then(|t| t.as_str());

    let schema = match type_str {
        Some("string") if v.get("enum").is_some() => {
            let variants: &'static [&'static str] = Box::leak(
                v["enum"]
                    .as_array()
                    .ok_or("enum must be an array")?
                    .iter()
                    .map(|e| -> Result<&'static str, String> {
                        let s = e.as_str().ok_or("enum variant must be a string")?;
                        Ok(Box::leak(s.to_owned().into_boxed_str()))
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .into_boxed_slice(),
            );
            ParamSchema::Enum {
                variants,
                description,
            }
        }
        Some("string") => ParamSchema::Primitive {
            kind: ParamKind::String,
            description,
        },
        Some("integer") => ParamSchema::Primitive {
            kind: ParamKind::Integer,
            description,
        },
        Some("number") => ParamSchema::Primitive {
            kind: ParamKind::Number,
            description,
        },
        Some("boolean") => ParamSchema::Primitive {
            kind: ParamKind::Bool,
            description,
        },
        Some("array") => {
            let items_val = v.get("items").ok_or("array schema missing items")?;
            let items: &'static ParamSchema = try_from_json(items_val)?;
            ParamSchema::Array { items, description }
        }
        Some("object") => {
            let props_map = v
                .get("properties")
                .and_then(|p| p.as_object())
                .ok_or("object schema missing properties")?;
            let required: Vec<&str> = v
                .get("required")
                .and_then(|r| r.as_array())
                .map(|arr| arr.iter().filter_map(|x| x.as_str()).collect())
                .unwrap_or_default();
            let properties: &'static [Property] = Box::leak(
                props_map
                    .iter()
                    .map(|(name, sub)| -> Result<Property, String> {
                        let static_name: &'static str = Box::leak(name.clone().into_boxed_str());
                        let static_schema: &'static ParamSchema = try_from_json(sub)?;
                        let is_required = required.contains(&name.as_str());
                        Ok((static_name, static_schema, is_required))
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .into_boxed_slice(),
            );
            ParamSchema::Object {
                properties,
                description,
            }
        }
        _ => ParamSchema::Any { description },
    };

    Ok(Box::leak(Box::new(schema)))
}

/// Pulls property names marked `"summary": true` out of a JSON schema.
/// Leaks like `try_from_json` so the result is `&'static`.
pub fn extract_summary_keys(v: &Value) -> &'static [&'static str] {
    let Some(props) = v.get("properties").and_then(|p| p.as_object()) else {
        return &[];
    };
    let keys: Vec<&'static str> = props
        .iter()
        .filter(|(_, sub)| sub.get("summary").and_then(|v| v.as_bool()) == Some(true))
        .map(|(name, _)| -> &'static str { Box::leak(name.clone().into_boxed_str()) })
        .collect();
    if keys.is_empty() {
        return &[];
    }
    Box::leak(keys.into_boxed_slice())
}

#[derive(Debug, Clone)]
enum PathSeg {
    Field(&'static str),
    Index(usize),
}

#[derive(Debug, Default, Clone)]
pub struct JsonPath(Vec<PathSeg>);

impl JsonPath {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn with_field<R>(&mut self, name: &'static str, f: impl FnOnce(&mut Self) -> R) -> R {
        self.0.push(PathSeg::Field(name));
        let out = f(self);
        self.0.pop();
        out
    }

    fn with_index<R>(&mut self, i: usize, f: impl FnOnce(&mut Self) -> R) -> R {
        self.0.push(PathSeg::Index(i));
        let out = f(self);
        self.0.pop();
        out
    }
}

impl Display for JsonPath {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for seg in &self.0 {
            match seg {
                PathSeg::Field(name) => {
                    if !first {
                        f.write_char('.')?;
                    }
                    f.write_str(name)?;
                }
                PathSeg::Index(i) => write!(f, "[{i}]")?,
            }
            first = false;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct ToolInputError {
    pub path: JsonPath,
    pub kind: ToolInputErrorKind,
}

#[derive(Debug)]
pub enum ToolInputErrorKind {
    Missing,
    TypeMismatch {
        expected: ParamKind,
        got: ParamKind,
        preview: Option<String>,
    },
    NotInEnum {
        expected: &'static [&'static str],
        got: String,
    },
    InternalBug {
        detail: String,
    },
}

impl ToolInputError {
    fn at(path: &JsonPath, kind: ToolInputErrorKind) -> Self {
        Self {
            path: path.clone(),
            kind,
        }
    }
}

impl Display for ToolInputError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str("invalid tool input: ")?;
        } else {
            write!(f, "invalid parameter '{}': ", self.path)?;
        }
        match &self.kind {
            ToolInputErrorKind::Missing => f.write_str("missing"),
            ToolInputErrorKind::TypeMismatch {
                expected,
                got,
                preview,
            } => {
                write!(f, "expected {expected}, got {got}")?;
                if let Some(p) = preview {
                    let hint = match expected {
                        ParamKind::Array => Some(JSON_ENCODED_ARRAY_HINT),
                        ParamKind::Object => Some(JSON_ENCODED_OBJECT_HINT),
                        _ => None,
                    };
                    if let Some(hint) = hint {
                        write!(f, ". {hint}")?;
                    }
                    write!(f, " Preview: {p}")?;
                }
                Ok(())
            }
            ToolInputErrorKind::NotInEnum { expected, got } => {
                f.write_str("expected one of [")?;
                for (i, v) in expected.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(v)?;
                }
                write!(f, "], got \"{got}\"")
            }
            ToolInputErrorKind::InternalBug { detail } => {
                write!(f, "internal validator bug: {detail}")
            }
        }
    }
}

pub(crate) fn preview(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(PARAM_PREVIEW_MAX) + 4);
    out.push('"');
    let mut written = 0usize;
    for ch in s.chars() {
        let escaped = match ch {
            '\n' => Some("\\n"),
            '\t' => Some("\\t"),
            '\r' => Some("\\r"),
            '\\' => Some("\\\\"),
            '"' => Some("\\\""),
            _ => None,
        };
        let chunk_len = escaped.map_or(ch.len_utf8(), str::len);
        if written + chunk_len > PARAM_PREVIEW_MAX {
            out.push_str(PREVIEW_SUFFIX);
            break;
        }
        match escaped {
            Some(s) => out.push_str(s),
            None => out.push(ch),
        }
        written += chunk_len;
    }
    out.push('"');
    out
}

pub fn validate(schema: &ParamSchema, input: Value) -> Result<Value, ToolInputError> {
    walk(schema, input, &mut JsonPath::default())
}

fn walk(schema: &ParamSchema, value: Value, path: &mut JsonPath) -> Result<Value, ToolInputError> {
    match schema {
        ParamSchema::Any { .. } => Ok(value),
        ParamSchema::Primitive { kind, .. } => validate_primitive(*kind, value, path),
        ParamSchema::Enum { variants, .. } => validate_enum(variants, value, path),
        ParamSchema::Array { items, .. } => validate_array(items, value, path),
        ParamSchema::Object { properties, .. } => validate_object(properties, value, path),
    }
}

fn validate_primitive(
    expected: ParamKind,
    value: Value,
    path: &mut JsonPath,
) -> Result<Value, ToolInputError> {
    let got = ParamKind::of(&value);
    if got == expected || (expected == ParamKind::Number && got == ParamKind::Integer) {
        return Ok(value);
    }
    if let Some(coerced) = coerce_primitive(&value, expected) {
        log_coercion(path, got, expected, &value, &coerced);
        return Ok(coerced);
    }
    Err(ToolInputError::at(
        path,
        ToolInputErrorKind::TypeMismatch {
            expected,
            got,
            preview: None,
        },
    ))
}

fn validate_enum(
    variants: &'static [&'static str],
    value: Value,
    path: &mut JsonPath,
) -> Result<Value, ToolInputError> {
    match &value {
        Value::String(s) if variants.contains(&s.as_str()) => Ok(value),
        Value::String(s) => Err(ToolInputError::at(
            path,
            ToolInputErrorKind::NotInEnum {
                expected: variants,
                got: preview(s),
            },
        )),
        other => Err(ToolInputError::at(
            path,
            ToolInputErrorKind::TypeMismatch {
                expected: ParamKind::String,
                got: ParamKind::of(other),
                preview: None,
            },
        )),
    }
}

fn validate_array(
    item_schema: &ParamSchema,
    value: Value,
    path: &mut JsonPath,
) -> Result<Value, ToolInputError> {
    let Value::Array(arr) = coerce_container(value, ParamKind::Array, path)? else {
        unreachable!("coerce_container(_, Array) returns an Array")
    };
    arr.into_iter()
        .enumerate()
        .map(|(i, item)| path.with_index(i, |p| walk(item_schema, item, p)))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

fn validate_object(
    properties: &'static [Property],
    value: Value,
    path: &mut JsonPath,
) -> Result<Value, ToolInputError> {
    let Value::Object(mut map) = coerce_container(value, ParamKind::Object, path)? else {
        unreachable!("coerce_container(_, Object) returns an Object")
    };
    let mut out = serde_json::Map::new();
    for (name, sub_schema, required) in properties {
        match map.remove(*name) {
            Some(v) if v.is_null() && !required => {}
            Some(v) => {
                let validated = path.with_field(name, |p| walk(sub_schema, v, p))?;
                out.insert((*name).into(), validated);
            }
            None if *required => {
                return Err(
                    path.with_field(name, |p| ToolInputError::at(p, ToolInputErrorKind::Missing))
                );
            }
            None => {}
        }
    }
    for (extra_key, _) in map {
        warn!(path = %path, key = %extra_key, "dropped unknown tool parameter");
    }
    Ok(Value::Object(out))
}

/// Models sometimes stringify arrays and objects, so we try parsing the
/// string as JSON before giving up.
fn coerce_container(
    value: Value,
    expected: ParamKind,
    path: &mut JsonPath,
) -> Result<Value, ToolInputError> {
    if ParamKind::of(&value) == expected {
        return Ok(value);
    }
    if let Value::String(s) = &value
        && let Some(parsed) = coerce_str_to(s, expected)
    {
        log_coercion(path, ParamKind::String, expected, &value, &parsed);
        return Ok(parsed);
    }
    let got = ParamKind::of(&value);
    let preview = if let Value::String(s) = &value {
        Some(preview(s))
    } else {
        None
    };
    Err(ToolInputError::at(
        path,
        ToolInputErrorKind::TypeMismatch {
            expected,
            got,
            preview,
        },
    ))
}

fn coerce_primitive(v: &Value, expected: ParamKind) -> Option<Value> {
    let s = v.as_str()?.trim();
    match expected {
        ParamKind::Integer => s.parse::<i64>().ok().map(Value::from),
        ParamKind::Number => s.parse::<f64>().ok().map(Value::from),
        ParamKind::Bool => match s {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        _ => None,
    }
}

fn coerce_str_to(s: &str, expected: ParamKind) -> Option<Value> {
    let parsed: Value = serde_json::from_str(s).ok()?;
    (ParamKind::of(&parsed) == expected).then_some(parsed)
}

fn log_coercion(
    path: &JsonPath,
    from: ParamKind,
    to: ParamKind,
    original: &Value,
    coerced: &Value,
) {
    warn!(
        path = %path,
        from = %from,
        to = %to,
        original = %preview(&original.to_string()),
        coerced = %preview(&coerced.to_string()),
        "coerced tool param type"
    );
}

/// Validate first so the LLM gets our structured errors, then hand off to
/// serde for defaults, renames, and tagged enums. If serde fails after
/// validation passed, the schema is out of sync with the Rust type, not
/// the model's fault, so we report `InternalBug`.
pub(crate) fn validate_and_deserialize<T: DeserializeOwned>(
    schema: &ParamSchema,
    input: Value,
) -> Result<T, ToolInputError> {
    let validated = validate(schema, input)?;
    serde_json::from_value(validated).map_err(|e| ToolInputError {
        path: JsonPath::default(),
        kind: ToolInputErrorKind::InternalBug {
            detail: e.to_string(),
        },
    })
}

#[cfg(test)]
pub(crate) const BOUNDED_ERR_MAX: usize = 400;

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const MSG_MISSING: &str = "missing";
    const MSG_EXPECTED_ARRAY: &str = "expected array";
    const MSG_JSON_ENCODED_HINT: &str = "Pass a JSON array";
    const MSG_EXPECTED_ONE_OF: &str = "expected one of";

    const STR_PRIM: ParamSchema = ParamSchema::Primitive {
        kind: ParamKind::String,
        description: "",
    };
    const BOOL_PRIM: ParamSchema = ParamSchema::Primitive {
        kind: ParamKind::Bool,
        description: "",
    };

    const EDIT_ENTRY: ParamSchema = ParamSchema::Object {
        properties: &[
            ("old_string", &STR_PRIM, true),
            ("new_string", &STR_PRIM, true),
            ("replace_all", &BOOL_PRIM, false),
        ],
        description: "",
    };

    const EDITS_ARRAY: ParamSchema = ParamSchema::Array {
        items: &EDIT_ENTRY,
        description: "",
    };

    const MULTIEDIT_LIKE: ParamSchema = ParamSchema::Object {
        properties: &[("path", &STR_PRIM, true), ("edits", &EDITS_ARRAY, true)],
        description: "",
    };

    const MODE_ENUM: ParamSchema = ParamSchema::Enum {
        variants: &["research", "general"],
        description: "",
    };

    #[test]
    fn param_kind_distinguishes_integer_from_number() {
        assert_eq!(ParamKind::of(&json!(3)), ParamKind::Integer);
        assert_eq!(ParamKind::of(&json!(1.5)), ParamKind::Number);
    }

    #[test]
    fn to_json_schema_object_is_closed_with_required_and_nested_items() {
        let v = to_json_schema(&MULTIEDIT_LIKE);
        assert_eq!(v["type"], "object");
        assert_eq!(v["additionalProperties"], false);
        let req: Vec<&str> = v["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(req, vec!["path", "edits"]);
        assert_eq!(v["properties"]["edits"]["type"], "array");
        assert_eq!(v["properties"]["edits"]["items"]["type"], "object");
        assert_eq!(
            v["properties"]["edits"]["items"]["additionalProperties"],
            false
        );
    }

    #[test]
    fn validate_missing_required_nested_has_dotted_path_and_display() {
        let err = validate(
            &MULTIEDIT_LIKE,
            json!({"path": "/x", "edits": [{"old_string": "a"}]}),
        )
        .unwrap_err();
        assert!(matches!(err.kind, ToolInputErrorKind::Missing));
        assert_eq!(err.path.to_string(), "edits[0].new_string");
        let rendered = err.to_string();
        assert!(rendered.contains(MSG_MISSING), "render: {rendered}");
    }

    #[test]
    fn coerce_stringified_json_array_is_accepted() {
        let input = json!({
            "path": "/x",
            "edits": r#"[{"old_string": "a", "new_string": "b"}]"#
        });
        let out = validate(&MULTIEDIT_LIKE, input).unwrap();
        assert_eq!(out["edits"][0]["old_string"], "a");
    }

    #[test]
    fn coerce_stringified_array_with_bad_inner_item_reports_nested_path() {
        let input = json!({
            "path": "/x",
            "edits": r#"[{"old_string": "a"}]"#
        });
        let err = validate(&MULTIEDIT_LIKE, input).unwrap_err();
        assert_eq!(err.path.to_string(), "edits[0].new_string");
        assert!(matches!(err.kind, ToolInputErrorKind::Missing));
    }

    #[test]
    fn huge_string_for_array_has_bounded_error_with_hint() {
        let huge: String = "x".repeat(50 * 1024);
        let input = json!({"path": "/x", "edits": huge});
        let err = validate(&MULTIEDIT_LIKE, input).unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.len() < BOUNDED_ERR_MAX, "too long: {rendered}");
        assert!(rendered.contains(MSG_EXPECTED_ARRAY));
        assert!(rendered.contains(MSG_JSON_ENCODED_HINT));
    }

    #[test_case(json!("30"),                     ParamKind::Integer, Some(json!(30))   ; "string_to_integer")]
    #[test_case(json!(" 42"),                    ParamKind::Integer, Some(json!(42))   ; "whitespace_trimmed")]
    #[test_case(json!("-5"),                     ParamKind::Integer, Some(json!(-5))   ; "negative")]
    #[test_case(json!(""),                       ParamKind::Integer, None              ; "empty_string")]
    #[test_case(json!("30, \"offset\": 2075"),   ParamKind::Integer, None              ; "embedded_trailing_fields_rejected")]
    #[test_case(json!("-3-5"),                   ParamKind::Integer, None              ; "malformed_number_rejected")]
    #[test_case(json!("1.25"),                   ParamKind::Number,  Some(json!(1.25)) ; "string_to_float")]
    #[test_case(json!("true"),                   ParamKind::Bool,    Some(json!(true)) ; "string_to_bool")]
    #[test_case(json!(30),                       ParamKind::Integer, None              ; "already_correct_type_no_coercion")]
    fn coerce_primitive_cases(value: Value, expected: ParamKind, wanted: Option<Value>) {
        assert_eq!(coerce_primitive(&value, expected), wanted);
    }

    #[test]
    fn preview_escapes_and_truncates_on_char_boundary() {
        assert_eq!(preview("a\nb\"c"), "\"a\\nb\\\"c\"");

        let long: String = "\u{1F600}".repeat(PARAM_PREVIEW_MAX);
        let out = preview(&long);
        assert!(out.ends_with(&format!("{PREVIEW_SUFFIX}\"")));
        assert!(out.len() <= PARAM_PREVIEW_MAX + PREVIEW_SUFFIX.len() + 2);
    }

    #[test]
    fn enum_errors_report_type_mismatch_and_render_variants() {
        let type_err = validate(&MODE_ENUM, json!(42)).unwrap_err();
        assert!(matches!(
            type_err.kind,
            ToolInputErrorKind::TypeMismatch {
                expected: ParamKind::String,
                got: ParamKind::Integer,
                ..
            }
        ));

        let value_err = validate(&MODE_ENUM, json!("human")).unwrap_err();
        let rendered = value_err.to_string();
        assert!(rendered.contains(MSG_EXPECTED_ONE_OF));
        assert!(rendered.contains("research"));
        assert!(rendered.contains("human"));
    }

    #[test]
    fn optional_null_treated_as_absent() {
        const SCHEMA: ParamSchema = ParamSchema::Object {
            properties: &[("name", &STR_PRIM, true), ("hint", &STR_PRIM, false)],
            description: "",
        };
        let out = validate(&SCHEMA, json!({"name": "x", "hint": null})).unwrap();
        assert_eq!(out["name"], "x");
        assert!(out.get("hint").is_none());
    }

    #[test]
    fn extra_keys_dropped() {
        const SCHEMA: ParamSchema = ParamSchema::Object {
            properties: &[("name", &STR_PRIM, true)],
            description: "",
        };
        let out = validate(&SCHEMA, json!({"name": "x", "extra": 42})).unwrap();
        assert!(out.get("extra").is_none());
    }

    #[test_case(ParamKind::String,  json!("hello"), json!(42)    ; "string_accepts_string_rejects_int")]
    #[test_case(ParamKind::Integer, json!(7),        json!("no") ; "integer_accepts_int_rejects_string")]
    #[test_case(ParamKind::Bool,    json!(true),     json!(1)    ; "bool_accepts_bool_rejects_int")]
    fn roundtrip_primitive(kind: ParamKind, good: Value, bad: Value) {
        let schema = ParamSchema::Primitive {
            kind,
            description: "",
        };
        let json_schema = to_json_schema(&schema);
        let recovered = try_from_json(&json_schema).expect("try_from_json failed");
        assert!(validate(recovered, good).is_ok());
        assert!(validate(recovered, bad).is_err());
    }

    #[test]
    fn roundtrip_object() {
        const INT_PRIM: ParamSchema = ParamSchema::Primitive {
            kind: ParamKind::Integer,
            description: "",
        };
        const SCHEMA: ParamSchema = ParamSchema::Object {
            properties: &[("name", &STR_PRIM, true), ("count", &INT_PRIM, false)],
            description: "",
        };
        let json_schema = to_json_schema(&SCHEMA);
        let recovered = try_from_json(&json_schema).expect("try_from_json failed");
        assert!(validate(recovered, json!({"name": "x", "count": 3})).is_ok());
        assert!(validate(recovered, json!({"name": "x"})).is_ok());
        assert!(validate(recovered, json!({"count": 3})).is_err());
    }
}
