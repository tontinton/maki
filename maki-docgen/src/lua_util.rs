use serde_json::{Map, Value, json};

pub fn find_matching_brace(s: &str, open: usize) -> Option<usize> {
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

pub fn extract_lua_field(s: &str, field: &str) -> Option<String> {
    let pattern = format!("{field} = \"");
    let start = s.find(&pattern)?;
    let after = &s[start + pattern.len()..];
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

fn unescape_lua_string(s: &str) -> String {
    s.replace("\\n", "\n")
}

pub fn extract_lua_description(source: &str) -> Option<String> {
    if let Some(start) = source.find("description = [[") {
        let after = &source[start + "description = [[".len()..];
        let end = after.find("]]")?;
        return Some(after[..end].trim().to_string());
    }
    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("description = ") {
            let rest = rest.trim_end_matches(',').trim();
            if !rest.starts_with('"') && !rest.starts_with("[[") {
                let var_pattern = format!("{rest} = [[");
                if let Some(vs) = source.find(&var_pattern) {
                    let after = &source[vs + var_pattern.len()..];
                    let end = after.find("]]")?;
                    return Some(after[..end].trim().to_string());
                }
            }
        }
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

pub struct LuaPluginCommand {
    pub name: String,
    pub description: String,
}

pub fn parse_lua_commands(source: &str) -> Vec<LuaPluginCommand> {
    let mut commands = Vec::new();
    let marker = "register_command({";
    let mut search = source;
    while let Some(start) = search.find(marker) {
        let block = &search[start + marker.len() - 1..];
        if let Some(end) = find_matching_brace(block, 0) {
            let inner = &block[1..end];
            let name = extract_lua_field(inner, "name");
            let desc = extract_lua_field(inner, "description");
            if let (Some(name), Some(description)) = (name, desc) {
                commands.push(LuaPluginCommand { name, description });
            }
            search = &block[end..];
        } else {
            break;
        }
    }
    commands
}

pub fn load_builtin_plugin_commands() -> Vec<LuaPluginCommand> {
    let Ok(entries) = std::fs::read_dir("plugins") else {
        return Vec::new();
    };
    let mut commands: Vec<LuaPluginCommand> = entries
        .filter_map(|e| e.ok())
        .flat_map(|e| {
            let path = e.path().join("init.lua");
            let source = std::fs::read_to_string(&path).ok()?;
            Some(parse_lua_commands(&source))
        })
        .flatten()
        .collect();
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    commands
}

pub fn load_builtin_plugin_tools() -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir("plugins") else {
        return Vec::new();
    };
    let mut plugins: Vec<Value> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path().join("init.lua");
            let source = std::fs::read_to_string(&path).ok()?;
            parse_lua_tool(&source)
        })
        .collect();
    plugins.sort_by(|a, b| {
        let na = a.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let nb = b.get("name").and_then(|n| n.as_str()).unwrap_or("");
        na.cmp(nb)
    });
    plugins
}

fn parse_lua_tool(source: &str) -> Option<Value> {
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

    if let Some(req_start) = schema_src.find("required = {") {
        let after = &schema_src[req_start + "required = {".len()..];
        if let Some(end) = after.find('}') {
            for item in after[..end].split(',') {
                let item = item.trim().trim_matches('"');
                if !item.is_empty() {
                    required.push(Value::String(item.to_string()));
                }
            }
        }
    }

    let props_start = schema_src.find("properties = {")?;
    let props_block = &schema_src[props_start..];
    let props_end = find_matching_brace(props_block, props_block.find('{')?)?;
    let props_src = &props_block["properties = {".len()..props_end];

    let mut pos = 0;
    while pos < props_src.len() {
        let remaining = &props_src[pos..];
        let Some(eq) = remaining.find('=') else { break };
        let pname = remaining[..eq]
            .trim()
            .trim_start_matches(|c: char| !c.is_ascii_alphabetic() && c != '_');
        if pname.is_empty() {
            pos += eq + 1;
            continue;
        }
        let after_eq = remaining[eq + 1..].trim_start();
        if !after_eq.starts_with('{') {
            pos += eq + 1;
            continue;
        }
        let brace_offset = pos + eq + 1 + (remaining.len() - eq - 1 - after_eq.len());
        let Some(brace_end) = find_matching_brace(props_src, brace_offset) else {
            break;
        };
        let block = &props_src[brace_offset..=brace_end];

        let ptype = extract_lua_field(block, "type").unwrap_or_default();
        let pdesc = extract_lua_field(block, "description").unwrap_or_default();
        if block.contains("required = true")
            && !required.contains(&Value::String(pname.to_string()))
        {
            required.push(Value::String(pname.to_string()));
        }

        if !ptype.is_empty() {
            properties.insert(
                pname.to_string(),
                json!({ "type": ptype, "description": pdesc }),
            );
        }
        pos = brace_end + 1;
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
