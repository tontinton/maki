use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_config::Permission;
use maki_lua::{LuaCommandInfo, PluginHost};
use maki_ui::{BUILTIN_COMMANDS, PluginAccess, STARTS_TURNS};

use crate::lua_util::in_registration_window;

const ALIASING: &str = r#"## Aliasing commands

Prefer a different name for a command? `maki.api.run_command` runs any slash command exactly as typing it would, so an alias is a one-line handler in your `init.lua` instead of a reimplementation.

```lua
-- ~/.config/maki/init.lua
local aliases = {
    { name = "/clear", target = "/new", description = "Alias for /new" },
    { name = "/resume", target = "/sessions", description = "Alias for /sessions" },
}

for _, alias in ipairs(aliases) do
    maki.api.register_command({
        name = alias.name,
        description = alias.description,
        handler = function()
            local ok, err = maki.api.run_command(alias.target)
            if not ok then
                maki.ui.flash("could not run " .. alias.target .. ": " .. err)
            end
        end,
    })
end
```

Aliasing adds a name, so both stay in the palette. Any command works as a target, including MCP prompts and other plugins' commands. See [`maki.api.run_command`](/docs/lua-api/#maki-api-run_command) for matching and error handling, or [`maki.ui.action`](/docs/lua-api/#maki-ui-action) to bind a key instead of a name."#;

const PLUGIN_ACCESS: &str = r#"### What a plugin may run {#plugin-access}

`run_command` only runs what the calling plugin's grants cover. Your `init.lua` counts as a plugin, and its grants come from `~/.config/maki/plugin.toml` (see [plugin permissions](/docs/lua-api/#plugin-permissions)). Commands not listed here run for any plugin."#;

const PLUGIN_COMMAND_ACCESS: &str = r#"A plugin command's handler runs with the grants of the plugin that registered it. The caller picks the arguments, so it must hold at least the same reach:

- Every permission the owner holds.
- Host access as wide as the owner's. A caller without `net_hosts` has it. A caller with a list needs the owner to have one too, and must contain each of its entries exactly as written, so `*.example.com` does not cover `api.example.com`.
- Its own provider on the address of each provider the owner registered. That address may be on your LAN, which `net_hosts` cannot grant.

A caller granted `run` skips both address checks, since a process it starts can reach any host."#;

fn builtin_names(access: PluginAccess) -> Vec<String> {
    BUILTIN_COMMANDS
        .iter()
        .filter(|cmd| cmd.plugin_access == access)
        .map(|cmd| format!("`{}`", cmd.name))
        .collect()
}

/// Read from booted plugins, so what is listed is the grant `run_command`
/// actually compares against.
fn bundled_commands() -> Vec<LuaCommandInfo> {
    let host = in_registration_window(|| {
        PluginHost::with_all_builtins(Arc::new(ToolRegistry::new())).expect("loading builtins")
    });
    let mut commands = host.command_reader().load().commands.clone();
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    commands
}

fn write_plugin_access(out: &mut String, bundled: &[LuaCommandInfo]) {
    writeln!(out, "{PLUGIN_ACCESS}").unwrap();
    writeln!(out).unwrap();
    let mut by_needs: BTreeMap<Vec<Permission>, Vec<String>> = BTreeMap::new();
    for &perm in Permission::ALL {
        let access = PluginAccess::Needs(perm);
        let names = by_needs.entry(vec![perm]).or_default();
        names.extend(builtin_names(access));
        if access == STARTS_TURNS {
            names.extend(["custom commands".into(), "MCP prompts".into()]);
        }
    }
    for cmd in bundled {
        let held: Vec<Permission> = Permission::ALL
            .iter()
            .copied()
            .filter(|&perm| cmd.reach.is_allowed(perm))
            .collect();
        if !held.is_empty() {
            by_needs
                .entry(held)
                .or_default()
                .push(format!("`{}`", cmd.name));
        }
    }
    for (perms, names) in by_needs.iter().filter(|(_, names)| !names.is_empty()) {
        let keys: Vec<String> = perms
            .iter()
            .map(|perm| format!("`{}`", perm.manifest_key()))
            .collect();
        writeln!(out, "- Needs {}: {}.", keys.join(", "), names.join(", ")).unwrap();
    }
    writeln!(
        out,
        "- Needs what its owner holds: every other plugin command, as described below."
    )
    .unwrap();
    writeln!(
        out,
        "- Only when you type it: {}. These change permissions, folder trust, or installed packages.",
        builtin_names(PluginAccess::UserOnly).join(", ")
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "{PLUGIN_COMMAND_ACCESS}").unwrap();
}

fn write_row(out: &mut String, name: &str, description: &str) {
    writeln!(out, "| `{name}` | {} |", description.replace('|', "\\|")).unwrap();
}

pub fn generate() -> String {
    let mut out = String::new();
    writeln!(out, "+++").unwrap();
    writeln!(out, "title = \"Commands\"").unwrap();
    writeln!(out, "weight = 8").unwrap();
    writeln!(out, "[extra]").unwrap();
    writeln!(out, "group = \"Reference\"").unwrap();
    writeln!(out, "+++").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "# Commands").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Type `/` in the input box to open the command palette."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "## Built-in commands").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| Command | Description |").unwrap();
    writeln!(out, "|---------|-------------|").unwrap();
    for cmd in BUILTIN_COMMANDS {
        write_row(&mut out, cmd.name, cmd.description);
    }
    let bundled = bundled_commands();
    for cmd in &bundled {
        write_row(&mut out, &cmd.name, &cmd.description);
    }

    writeln!(out).unwrap();
    writeln!(out, "## Sessions").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Sessions run concurrently. `/new` starts a fresh session while the old one keeps working in the background, and `/sessions` shows the live status of each (working, needs input, idle) so you can jump between them. When a background session finishes or needs input, Maki flashes a note in the status bar. `/rename` renames the current session; in the session picker, `Ctrl+N` / `Ctrl+R` / `Ctrl+D` create, rename, and delete. Deleting the current session starts a fresh one and keeps the picker open."
    )
    .unwrap();

    writeln!(out).unwrap();
    writeln!(out, "## Modes and toggles").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "- **`/yolo`**: skip permission prompts for this session (deny rules still apply). The toggle survives a resume, and `--yolo` only sets the starting value. Config: `always_yolo = true`."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/thinking`**: extended thinking. Bare, or `Alt+T`, it opens a picker that shows what each level costs in tokens. With an argument it sets the level directly: `off`, `adaptive`, `minimal` to `max`, or a token budget. New sessions start at the last level you set, in `-p`, SDK and ACP too. Config: `always_thinking` pins a level."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/fast`**: faster responses on Anthropic Opus, and on eligible Codex models when you sign in with a ChatGPT subscription. OpenAI API keys and every other model ignore it. Config: `always_fast = true`."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/workflow`**: let `code_execution` call the `task` tool (and other workflow-only tools) from inside the Python sandbox. Config: `always_workflow = true`."
    )
    .unwrap();
    writeln!(
        out,
        "- **Plan / build**: not a slash command. Press `Tab` in the input to toggle plan mode (plan-file writes only)."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/reload`**: rebuild plugins and config without leaving the app."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/btw`**: one-shot side question with no tools and no history pollution."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/memory`**: open the memory file picker (view / edit / delete). See the `memory` tool under [Tools](/docs/tools/)."
    )
    .unwrap();

    writeln!(out).unwrap();
    writeln!(out, "## Custom commands").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "You can define your own slash commands as Markdown files. Empty files are skipped."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### Discovery and priority").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Later sources override earlier ones when the command **name** matches (the stem of the file, or `name` in frontmatter):"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "1. User config: `~/.config/maki/commands/` (and legacy `~/.maki/commands/` if present)"
    )
    .unwrap();
    writeln!(out, "2. User third-party: `~/.claude/commands/`").unwrap();
    writeln!(
        out,
        "3. Project dirs, walking from the current working directory up to the nearest `.git` root. At each level: `.maki/commands/`, then `.claude/commands/`"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Because the walk goes cwd → … → git root, a command at the **repository root overrides** the same name found only under a nested cwd. Project commands override user commands. Palette names are `/project:<name>` or `/user:<name>` depending on which scope won."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Skip all of the above with `--no-commands` (see [CLI](/docs/cli/))."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### Metadata").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "You can add optional metadata at the top of the file between `---` lines to set `name`, `description`, and `argument-hint`:"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "```markdown").unwrap();
    writeln!(out, "---").unwrap();
    writeln!(out, "description: Review code for issues").unwrap();
    writeln!(out, "argument-hint: <file>").unwrap();
    writeln!(out, "---").unwrap();
    writeln!(out, "Review $ARGUMENTS and suggest improvements.").unwrap();
    writeln!(out, "```").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### Arguments").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Use `$ARGUMENTS` in the command body. It gets replaced with whatever you type after the command name. The command is treated as accepting args if the body contains `$ARGUMENTS` or you set `argument-hint`."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "For example, `/project:review main.rs` replaces `$ARGUMENTS` with `main.rs`."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "{ALIASING}").unwrap();
    writeln!(out).unwrap();
    write_plugin_access(&mut out, &bundled);

    writeln!(out).unwrap();
    writeln!(
        out,
        "Related: [CLI](/docs/cli/) for shell flags and subcommands, [Skills](/docs/skills/) for on-demand playbooks."
    )
    .unwrap();

    if out.ends_with('\n') {
        out.pop();
    }
    out
}
