use maki_agent::template::Vars;
use maki_agent::tools::{DescriptionContext, ToolFilter, ToolRegistry};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
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
    (
        "Agent & Knowledge",
        &["task", "todo_write", "memory", "skill"],
    ),
    (
        "LSP",
        &[
            "lsp_goto_definition",
            "lsp_references",
            "lsp_hover",
            "lsp_diagnostics",
            "lsp_goto_implementation",
            "lsp_document_symbol",
            "lsp_workspace_symbol",
            "lsp_incoming_calls",
            "lsp_outgoing_calls",
        ],
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

fn parse_lua_plugin(source: &str) -> Option<Value> {
    let name = source
        .lines()
        .find_map(|l| l.trim().strip_prefix("name = \"")?.strip_suffix("\","))?;

    let desc = extract_lua_description(source)?;

    let schema_start = source.find("schema = {")?;
    let schema_block = &source[schema_start..];
    let schema_end = find_matching_brace(schema_block, schema_block.find('{')?)?;
    let schema_src = &schema_block[..=schema_end];

    let mut properties = Map::new();
    let mut required = Vec::new();

    let props_start = schema_src.find("properties = {")?;
    let props_block = &schema_src[props_start..];
    let props_end = find_matching_brace(props_block, props_block.find('{')?)?;
    let props_src = &props_block["properties = {".len()..props_end];

    for line in props_src.lines() {
        let line = line.trim();
        let Some((pname, rest)) = line.split_once('=') else {
            continue;
        };
        let pname = pname.trim();
        if !rest.trim().starts_with('{') {
            continue;
        }

        let ptype = extract_lua_field(rest, "type")?;
        let pdesc = extract_lua_field(rest, "description").unwrap_or_default();
        let is_required = rest.contains("required = true");

        let prop = json!({ "type": ptype, "description": pdesc });
        if is_required {
            required.push(Value::String(pname.to_string()));
        }
        properties.insert(pname.to_string(), prop);
    }

    let schema = json!({
        "type": "object",
        "required": required,
        "properties": properties,
        "additionalProperties": false,
    });

    Some(json!({
        "name": name,
        "description": desc,
        "input_schema": schema,
    }))
}

fn extract_lua_description(source: &str) -> Option<String> {
    if let Some(start) = source.find("description = [[") {
        let after = &source[start + "description = [[".len()..];
        let end = after.find("]]")?;
        return Some(after[..end].trim().to_string());
    }
    let marker = "description = \"";
    let start = source.find(marker)?;
    let desc_block = &source[start..];
    let mut parts = Vec::new();
    for line in desc_block.lines() {
        let trimmed = line.trim();
        let quoted = trimmed
            .strip_prefix(".. \"")
            .or_else(|| trimmed.strip_prefix("description = \""));
        if let Some(s) = quoted {
            if let Some(end) = s.rfind('"') {
                parts.push(unescape_lua_string(&s[..end]));
            }
        }
        if !trimmed.contains("..") && trimmed.ends_with(',') {
            break;
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join(""))
}

fn unescape_lua_string(s: &str) -> String {
    s.replace("\\n", "\n")
}

fn find_matching_brace(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    for (i, ch) in s[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

fn extract_lua_field(s: &str, field: &str) -> Option<String> {
    let pattern = format!("{field} = \"");
    let start = s.find(&pattern)?;
    let after = &s[start + pattern.len()..];
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

fn load_builtin_plugins() -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir("plugins") else {
        return Vec::new();
    };
    let mut plugins: Vec<Value> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path().join("init.lua");
            let source = std::fs::read_to_string(&path).ok()?;
            parse_lua_plugin(&source)
        })
        .collect();
    plugins.sort_by(|a, b| {
        let na = a.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let nb = b.get("name").and_then(|n| n.as_str()).unwrap_or("");
        na.cmp(nb)
    });
    plugins
}

pub fn generate() -> String {
    let vars = Vars::new()
        .set("{cwd}", "<cwd>")
        .set("{platform}", "linux")
        .set("{date}", "YYYY-MM-DD");
    let defs = ToolRegistry::native().definitions(
        &vars,
        &DescriptionContext {
            skills: &[],
            filter: &ToolFilter::All,
        },
        false,
    );
    let plugin_defs = load_builtin_plugins();
    let plugin_names: HashSet<&str> = plugin_defs
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    let all_tools: Vec<&Value> = defs
        .as_array()
        .expect("definitions should be an array")
        .iter()
        .chain(&plugin_defs)
        .collect();

    let tool_map: HashMap<&str, &Value> = all_tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(|n| (n, *t)))
        .collect();

    let total = all_tools.len();
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
            if plugin_names.contains(name) {
                writeln!(out, "### `{name}` *(lua plugin)*").unwrap();
            } else {
                writeln!(out, "### `{name}`").unwrap();
            }
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
