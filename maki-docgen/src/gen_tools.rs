use maki_agent::template::Vars;
use maki_agent::tools::{DescriptionContext, ToolAudience, ToolFilter, ToolRegistry, ToolSource};
use maki_config::OPT_IN_TOOLS;
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::sync::Arc;

use maki_lua::PluginHost;

const DATE_PLACEHOLDER: &str = "YYYY-MM-DD";

const SECTIONS: &[(&str, &[&str])] = &[
    (
        "File Operations",
        &[
            "bash",
            "read",
            "write",
            "edit",
            "multiedit",
            "edit_lines",
            "glob",
            "grep",
            "index",
            "view_image",
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
    ("Web", &["webfetch", "websearch"]),
];

struct ToolInfo {
    def: Value,
    source: ToolSource,
}

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
    let header = if has_defaults {
        "| Parameter | Type | Required | Default | Description |\n|-----------|------|----------|---------|-------------|"
    } else {
        "| Parameter | Type | Required | Description |\n|-----------|------|----------|-------------|"
    };
    writeln!(out, "{header}").unwrap();
    for p in params {
        let desc = p.description.replace('\n', "<br>");
        let required = if p.required { "yes" } else { "no" };
        if has_defaults {
            writeln!(
                out,
                "| `{}` | {} | {} | {} | {} |",
                p.name, p.ty, required, p.default, desc
            )
            .unwrap();
        } else {
            writeln!(out, "| `{}` | {} | {} | {} |", p.name, p.ty, required, desc).unwrap();
        }
    }
}

fn source_label(source: &ToolSource) -> &'static str {
    match source {
        ToolSource::Native => "native",
        ToolSource::Lua { .. } => "lua plugin",
        ToolSource::Mcp { .. } => "mcp",
    }
}

fn write_tool_entry(out: &mut String, name: &str, info: &ToolInfo) {
    let description = info
        .def
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");
    let schema = info.def.get("input_schema").cloned().unwrap_or(Value::Null);
    let params = extract_params(&schema);
    let summary = first_paragraph(description);

    writeln!(out).unwrap();
    let mut badge_text = source_label(&info.source).to_string();
    if OPT_IN_TOOLS.contains(&name) {
        badge_text.push_str(", opt-in");
    }
    writeln!(out, "### `{name}` *({badge_text})*").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "{summary}").unwrap();
    writeln!(out).unwrap();
    write_param_table(out, &params);
}

/// Replace `target` with `placeholder`. Empty `target` is a no-op.
fn redact_path(input: &str, target: &str, placeholder: &str) -> String {
    if target.is_empty() {
        input.to_string()
    } else {
        input.replace(target, placeholder)
    }
}

/// Plugins bake env-specific values into their `description` at registration:
/// `bash` interpolates `maki.uv.cwd()` and `websearch` interpolates
/// `os.date("%Y-%m-%d")`. Scrub both so `gen-docs-check` is stable across
/// machines and days. CWD is replaced before HOME so a cwd nested under ~
/// doesn't get partially mangled.
fn redact_env_and_dates(input: &str) -> String {
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|c| c.to_str().map(str::to_owned))
        .unwrap_or_default();
    let mut out = redact_path(input, &cwd, "<cwd>");
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        out = redact_path(&out, &home, "~");
    }
    DATE_RE.replace_all(&out, DATE_PLACEHOLDER).into_owned()
}

fn redact_def(def: &Value) -> Value {
    let Some(d) = def.get("description").and_then(|v| v.as_str()) else {
        return def.clone();
    };
    let redacted = redact_env_and_dates(d);
    if redacted == d {
        def.clone()
    } else {
        let mut out = def.clone();
        out["description"] = Value::String(redacted);
        out
    }
}

fn write_front_matter(out: &mut String) {
    writeln!(out, "+++").unwrap();
    writeln!(out, "title = \"Tools\"").unwrap();
    writeln!(out, "weight = 3").unwrap();
    writeln!(out, "[extra]").unwrap();
    writeln!(out, "group = \"Reference\"").unwrap();
    writeln!(out, "+++").unwrap();
}

fn collect_tool_info(
    def_map: &HashMap<String, &Value>,
    entry: &maki_agent::tools::RegisteredTool,
) -> Option<ToolInfo> {
    let name = entry.name();
    let def = def_map.get(name)?;
    Some(ToolInfo {
        def: redact_def(def),
        source: entry.source.clone(),
    })
}

fn load_registry_with_builtins() -> Arc<ToolRegistry> {
    let registry = Arc::new(ToolRegistry::with_natives());
    let _host =
        PluginHost::with_all_builtins(Arc::clone(&registry)).expect("loading builtin plugins");
    registry
}

pub fn generate() -> String {
    let vars = Vars::new()
        .set("{cwd}", "<cwd>")
        .set("{platform}", "linux")
        .set("{date}", "YYYY-MM-DD");

    let registry = load_registry_with_builtins();
    let defs = registry.definitions(
        &vars,
        &DescriptionContext {
            filter: &ToolFilter::All,
            audience: ToolAudience::MAIN,
            workflow: false,
        },
        false,
    );
    let def_map: HashMap<String, &Value> = defs
        .as_array()
        .expect("definitions should be an array")
        .iter()
        .filter_map(|t| {
            t.get("name")
                .and_then(|n| n.as_str())
                .map(|n| (n.to_string(), t))
        })
        .collect();

    let snapshot = registry.iter();
    let mut tools: HashMap<&str, ToolInfo> = HashMap::new();
    for entry in snapshot.iter() {
        if let Some(info) = collect_tool_info(&def_map, entry) {
            tools.insert(entry.name(), info);
        }
    }

    let total = tools.len();
    let mut out = String::new();
    write_front_matter(&mut out);
    writeln!(out).unwrap();
    writeln!(out, "# Tools").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Maki ships with {total} built-in tools. This is the full reference."
    )
    .unwrap();

    let mut rendered: HashSet<&str> = HashSet::new();

    for (section_name, tool_names) in SECTIONS {
        let present: Vec<&str> = tool_names
            .iter()
            .copied()
            .filter(|n| tools.contains_key(*n))
            .collect();
        if present.is_empty() {
            continue;
        }
        writeln!(out).unwrap();
        writeln!(out, "## {section_name}").unwrap();
        for name in present {
            let info = tools.get(name).expect("checked above");
            write_tool_entry(&mut out, name, info);
            rendered.insert(name);
        }
    }

    let mut leftovers: Vec<&str> = tools
        .keys()
        .filter(|n| !rendered.contains(*n))
        .copied()
        .collect();
    leftovers.sort_unstable();
    if !leftovers.is_empty() {
        writeln!(out).unwrap();
        writeln!(out, "## Additional tools").unwrap();
        for name in leftovers {
            let info = tools.get(name).expect("checked above");
            write_tool_entry(&mut out, name, info);
        }
    }

    if out.ends_with('\n') {
        out.pop();
    }
    out
}

static DATE_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"\d{4}-\d{2}-\d{2}").expect("valid date regex"));

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("2026-07-05", "YYYY-MM-DD"; "simple date")]
    #[test_case("today is 2026-07-05 here", "today is YYYY-MM-DD here"; "embedded date")]
    #[test_case("v2024-01-15", "vYYYY-MM-DD"; "prefix embedded")]
    #[test_case("2026-7-5", "2026-7-5"; "single digit not matched")]
    #[test_case("no date here", "no date here"; "no date")]
    #[test_case("2026-07-05 and 2025-12-31", "YYYY-MM-DD and YYYY-MM-DD"; "two dates")]
    fn redacts_date(input: &str, expected: &str) {
        // redact_env_and_dates also scrubs HOME/cwd; isolate date logic by
        // calling the regex replacement directly.
        assert_eq!(
            DATE_RE.replace_all(input, DATE_PLACEHOLDER).as_ref(),
            expected
        );
    }

    #[test_case("/home/user/repo", "/home/user", "~", "~/repo"; "path under home")]
    #[test_case("/home/user", "/home/user", "~", "~"; "exact home")]
    #[test_case("/elsewhere", "/home/user", "~", "/elsewhere"; "unrelated path")]
    #[test_case("any", "", "<cwd>", "any"; "empty target no-op")]
    #[test_case("", "/home/user", "~", ""; "empty input")]
    fn redacts_path(input: &str, target: &str, placeholder: &str, expected: &str) {
        assert_eq!(redact_path(input, target, placeholder), expected);
    }

    #[test_case("$(default: foo)", "foo", "$"; "colon form")]
    #[test_case("desc (default bar) suffix", "bar", "desc suffix"; "space form")]
    #[test_case("prefix (default: baz) extra", "baz", "prefix extra"; "in middle")]
    #[test_case("plain description", "", "plain description"; "no default")]
    fn extracts_default(input: &str, expected_default: &str, remaining_prefix: &str) {
        let (default, cleaned) = extract_default(input);
        assert_eq!(default, expected_default);
        assert!(cleaned.starts_with(remaining_prefix), "cleaned: {cleaned}");
    }

    #[test]
    fn sections_partition_registered_tools() {
        let registry = load_registry_with_builtins();
        let snapshot = registry.iter();
        let registered: HashSet<&str> = snapshot.iter().map(|e| e.name()).collect();

        let mut sectioned: HashSet<&str> = HashSet::new();
        for (_, names) in SECTIONS {
            for &n in *names {
                assert!(
                    registered.contains(n),
                    "SECTIONS references \"{n}\" which isn't a registered tool"
                );
                assert!(sectioned.insert(n), "\"{n}\" appears in multiple sections");
            }
        }

        let unsectioned: Vec<&str> = registered.difference(&sectioned).copied().collect();
        assert!(
            unsectioned.is_empty(),
            "registered tools missing from SECTIONS: {unsectioned:?}"
        );
    }
}
