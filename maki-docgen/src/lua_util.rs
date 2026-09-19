use std::sync::Mutex;

/// Runs a plugin load inside its own provider registration window.
///
/// Booting a host stages the providers its bundled plugins declare into one
/// registry the whole process shares, and docgen builds each page on its own
/// thread. Keeping a single window open at a time stops two hosts from staging
/// over each other and fighting for the same slug.
pub fn in_registration_window<T>(load: impl FnOnce() -> T) -> T {
    static WINDOW: Mutex<()> = Mutex::new(());
    let _guard = WINDOW.lock().unwrap_or_else(|e| e.into_inner());
    maki_providers::plugin::begin_load();
    let loaded = load();
    maki_providers::plugin::commit_load();
    loaded
}

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
    let dq = format!("{field} = \"");
    let sq = format!("{field} = '");
    if let Some(start) = s.find(&dq) {
        let after = &s[start + dq.len()..];
        let end = after.find('"')?;
        Some(unescape_lua_string(&after[..end]))
    } else {
        let start = s.find(&sq)?;
        let after = &s[start + sq.len()..];
        let end = after.find('\'')?;
        Some(unescape_lua_string(&after[..end]))
    }
}

fn unescape_lua_string(s: &str) -> String {
    s.replace("\\n", "\n")
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
        .flat_map(|plugin| {
            std::fs::read_dir(plugin.path())
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "lua"))
                .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                .flat_map(|source| parse_lua_commands(&source))
                .collect::<Vec<_>>()
        })
        .collect();
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    commands
}
