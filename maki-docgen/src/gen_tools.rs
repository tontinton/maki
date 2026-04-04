use maki_agent::template::Vars;
use maki_agent::tools::{DescriptionContext, ToolCall, ToolFilter};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt::Write;

const SECTIONS: &[(&str, &[&str])] = &[
    (
        "File Operations",
        &[
            "bash",
            "read",
            "write",
            "edit",
            "multiedit",
            "glob",
            "grep",
            "index",
        ],
    ),
    (
        "Execution & Control",
        &["batch", "code_execution", "question"],
    ),
    ("External", &["webfetch", "websearch"]),
    (
        "Agent & Knowledge",
        &["task", "todowrite", "memory", "skill"],
    ),
];

struct Param {
    name: String,
    ty: String,
    required: bool,
    default: String,
    description: String,
}

fn extract_default(desc: &str) -> (String, String) {
    for pattern in ["(default: ", "(default "] {
        if let Some(start) = desc.find(pattern) {
            let after = &desc[start + pattern.len()..];
            if let Some(end) = after.find(')') {
                let default_val = after[..end].to_string();
                let cleaned = format!(
                    "{}{}",
                    desc[..start].trim_end(),
                    &desc[start + pattern.len() + end + 1..]
                )
                .trim()
                .to_string();
                return (default_val, cleaned);
            }
        }
    }
    (String::new(), desc.to_string())
}

fn first_paragraph(desc: &str) -> &str {
    desc.split("\n\n").next().unwrap_or(desc)
}

fn extract_params(schema: &Value) -> Vec<Param> {
    let properties = match schema.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut params = Vec::new();
    for (name, prop) in properties {
        let raw_type = prop
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("string");
        let raw_desc = prop
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        let is_required = required.contains(&name.as_str());
        let (default, description) = extract_default(raw_desc);
        params.push(Param {
            name: name.clone(),
            ty: raw_type.to_string(),
            required: is_required,
            default,
            description,
        });
    }
    params
}

fn write_param_table(out: &mut String, params: &[Param]) {
    let has_defaults = params.iter().any(|p| !p.default.is_empty());

    if has_defaults {
        writeln!(
            out,
            "| Parameter | Type | Required | Default | Description |"
        )
        .unwrap();
        writeln!(
            out,
            "|-----------|------|----------|---------|-------------|"
        )
        .unwrap();
        for p in params {
            let desc = p.description.replace('\n', "<br>");
            writeln!(
                out,
                "| `{}` | {} | {} | {} | {} |",
                p.name,
                p.ty,
                if p.required { "yes" } else { "no" },
                p.default,
                desc
            )
            .unwrap();
        }
    } else {
        writeln!(out, "| Parameter | Type | Required | Description |").unwrap();
        writeln!(out, "|-----------|------|----------|-------------|").unwrap();
        for p in params {
            let desc = p.description.replace('\n', "<br>");
            writeln!(
                out,
                "| `{}` | {} | {} | {} |",
                p.name,
                p.ty,
                if p.required { "yes" } else { "no" },
                desc
            )
            .unwrap();
        }
    }
}

pub fn generate() -> String {
    let vars = Vars::new()
        .set("{cwd}", "<cwd>")
        .set("{platform}", "linux")
        .set("{date}", "YYYY-MM-DD");
    let defs = ToolCall::definitions(
        &vars,
        &DescriptionContext {
            skills: &[],
            filter: &ToolFilter::All,
        },
        false,
    );
    let tools: Vec<&Value> = defs
        .as_array()
        .expect("definitions should be an array")
        .iter()
        .collect();

    let tool_map: HashMap<&str, &Value> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(|n| (n, *t)))
        .collect();

    let total = tools.len();
    let mut out = String::new();
    writeln!(out, "+++").unwrap();
    writeln!(out, "title = \"Tools\"").unwrap();
    writeln!(out, "weight = 3").unwrap();
    writeln!(out, "[extra]").unwrap();
    writeln!(out, "group = \"Reference\"").unwrap();
    writeln!(out, "+++").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "# Tools").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Maki ships with {total} built-in tools. This is the full reference."
    )
    .unwrap();

    for (section_name, tool_names) in SECTIONS {
        writeln!(out).unwrap();
        writeln!(out, "## {section_name}").unwrap();

        for name in *tool_names {
            let Some(tool) = tool_map.get(name) else {
                continue;
            };
            let description = tool
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            let schema = tool.get("input_schema").cloned().unwrap_or(Value::Null);
            let params = extract_params(&schema);
            let summary = first_paragraph(description);

            writeln!(out).unwrap();
            writeln!(out, "### `{name}`").unwrap();
            writeln!(out).unwrap();
            writeln!(out, "{summary}").unwrap();
            writeln!(out).unwrap();
            write_param_table(&mut out, &params);
        }
    }

    if out.ends_with('\n') {
        out.pop();
    }
    out
}
