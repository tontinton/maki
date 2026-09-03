//! Renders the Lua API reference and the builtin `maki-plugin-dev` skill
//! straight from `api_docs()` at runtime, so no generated files live in the
//! repo. `maki-docgen` calls [`site_page`] for the website; the plugin
//! `require()` sandbox serves [`virtual_module`] to the skill plugin.

use mlua::{Lua, Table};

use crate::docs::{DocKind, FnDoc, ModuleDoc, api_docs};
use crate::loader::lib_dir;
use crate::plugin_permissions::Permission;

const SKILL_MODULE: &str = "plugin_dev";
const REFERENCE_MODULE: &str = "plugin_dev_reference";

const NAME: &str = "maki-plugin-dev";
const DESCRIPTION: &str = "Write or modify maki plugins or init.lua config in Lua: custom tools, slash commands, keymaps, UI. Authoring guide, real example, indexed maki Lua API reference. Load before any maki plugin work.";
const REFERENCE_PLACEHOLDER: &str = "__MAKI_REFERENCE_PATH__";

const EXAMPLE: &str = include_str!("../../plugins/glob/init.lua");

const PERMISSIONS_ANCHOR: &str = "plugin-permissions";
const REFERENCE_URL: &str = "/docs/lua-api/";

/// Generated from [`Permission::ALL`] so the reference can never drift from
/// the real gate set. `anchored` adds the explicit heading id for the site;
/// the skill copy stays plain markdown.
fn permissions_section(anchored: bool) -> String {
    const TEMPLATE: &str = r#"## Permissions and plugin.toml{ANCHOR}

Sensitive APIs are gated per plugin file, and every gated function's entry in
this reference names the permission it needs. A gated call without its
permission raises `permission denied: '<name>' not granted for this plugin`.

{NAMES}

Grants come from a `plugin.toml` next to the Lua file (for
`~/.config/maki/init.lua` that is `~/.config/maki/plugin.toml`):

```toml
min_maki_version = "0.4.12"

[permissions]
{KEYS}```

The rules:

- No `plugin.toml` at all: every permission is denied, and maki logs a
  warning at load time.
- `plugin.toml` exists: permissions default to granted; set a key to
  `false` to revoke it. An empty file grants everything.
- Invalid TOML: everything denied, with a warning in the log.
- A package, or a plugin maki ships, is read the other way round: a key it
  does not name is not requested, so its `plugin.toml` lists everything it
  uses. Only a `plugin.toml` you wrote yourself defaults to granted.
- `min_maki_version` is optional and takes a plain semantic version as a lower
  bound, so ranges do not work. When the field is invalid or the running
  version is older, Maki skips the Lua in that directory and warns at startup
  instead of failing. The same floor applies to an installed package, which is
  skipped while the rest keep loading. `--no-plugins` still skips every user
  plugin at once.
"#;
    let anchor = if anchored {
        format!(" {{#{PERMISSIONS_ANCHOR}}}")
    } else {
        String::new()
    };
    let names = Permission::ALL
        .iter()
        .map(|p| format!("- `{}`: {}\n", p.manifest_key(), p.describes()))
        .collect::<String>();
    let keys = Permission::ALL
        .iter()
        .map(|p| format!("{} = true\n", p.manifest_key()))
        .collect::<String>();
    TEMPLATE
        .replace("{ANCHOR}", &anchor)
        .replace("{NAMES}", names.trim_end())
        .replace("{KEYS}", &keys)
}

const GUIDE: &str = r#"# Writing maki plugins

Maki plugins are plain Lua files (Luau) that run inside maki. A plugin can
register tools the LLM calls, slash commands, keymaps, prompt hints, and
custom UI. Everything lives under the global `maki` table. The full API
reference is at the end of this document.

## Where plugin code goes

Plugins live in the maki config dir. There are two of them, same layout:

- `~/.config/maki/` - global, every project (if `~/.maki/` exists, maki reads
  that one first)
- `<project>/.maki/` - this project only

```
init.lua        the only file maki runs; require()s plugins, calls maki.setup()
lua/<name>.lua  plugin modules, loaded by require("<name>")
plugin.toml     permission grants for every Lua file in the dir
```

Nothing under `lua/` loads on its own. A module name is its path under `lua/`
without the extension: `lua/browser.lua` is `require("browser")`,
`lua/acme/tools.lua` is `require("acme.tools")`. `require` is sandboxed to
that directory, you cannot reach files outside it.

## Creating a plugin

1. Write the code in `~/.config/maki/lua/<name>.lua`. The `maki` global is
   already there, nothing to import. For a project-only plugin use
   `<project>/.maki/` here and in every step below.

```lua
maki.api.register_tool({
  name = "hello",
  description = "Say hello to a name.",
  parameters = { type = "object", properties = { name = { type = "string" } }, required = { "name" } },
  handler = function(args)
    return { llm_output = "hello " .. args.name }
  end,
})
```

2. Load it from `~/.config/maki/init.lua`, creating that file if missing:

```lua
require("hello")
```

3. Grant the permissions it needs in `~/.config/maki/plugin.toml`, creating
   that file if missing. Without the file every gated call is denied.

```toml
[permissions]
fs_read = true
run = true
```

4. Run `/reload`, then read the log as described below, to see that it loaded
   and what it printed.

Leave `maki.api.register_options` to bundled plugins: maki rejects a
`plugins.<name>` table for a plugin it does not ship, and startup fails. Keep
settings in a local table, or export a `setup(opts)` function `init.lua` calls.

{PERMISSIONS}

## Development loop

`/reload` rebuilds plugins and config in place, no restart needed. Until it
runs, an edited plugin is still the old one.

To debug, add `maki.log.info|warn|error(...)` calls. They write to `maki.log`
in the dir `maki.env.logs_dir()` returns (Linux: `~/.local/logs/maki/`). When
a backtrace comes out useless, start maki with `--no-jit`: plugins then run on
the interpreter, with full debug info.

{AGENT_NOTES}## Conventions

- Fallible runtime calls return a `(value, err)` pair; check `err` before using `value`.
- Tool handlers report failures with `{ llm_output = "error: ...", is_error = true }`, not by raising.
- The model picks tools by reading `description`, so state precisely what the tool does and when to use it.
- Reusable helpers ship with maki; see "Shared helper modules" in the API reference.

## A complete real example

The bundled `glob` tool, verbatim: schema, header and restore hooks, error
handling, LLM output truncation, collapsible UI view. It is a bundled plugin,
so it opens with `register_options`, which your own plugin skips:

```lua
"#;

/// Everything that only makes sense to the agent lives here, so the website
/// page reads like a page and not like someone else's instructions. The blank
/// line at the end belongs to the block: the slot sits flush against the next
/// heading, so leaving it out leaves no hole.
const AGENT_NOTES: &str = r#"## Notes for the agent

- Never write a plugin into maki's own source tree. The `plugins/` directory
  of the maki repo holds the plugins that ship with maki, compiled into the
  binary, so a file dropped there does nothing until maki is rebuilt. That
  holds even when the project you have open is a maki checkout.
- Both global config dirs can exist, and `~/.maki/` wins, so look before you
  write.
- The config dir sits outside the project, but it is an ordinary directory:
  create files there with the normal write and edit tools.
- You cannot run slash commands or restart maki, so ask the user to run
  `/reload` and to reproduce the problem, then read the log yourself.

"#;

const HEADER: &str = r#"# Lua API

Maki plugins are plain Lua files. Everything a plugin can touch lives under
one global table: `maki`. This reference documents every module, function,
and method. It is generated straight from the source code by `maki-docgen`.
For where plugin files live and how to load them, read the
[Plugins guide](/docs/plugins/) first.

The API tries to mirror Neovim as much as possible (`maki.fs`, `maki.uv`,
`maki.treesitter`, `maki.keymap`, `maki.base64`), signatures are kept identical
so code can be copy-pasted between the two without too many modifications.

Plugins run compiled to native code (Luau JIT). If you are debugging a
plugin and want full backtraces, start maki with `--no-jit`: it runs your
Lua on the interpreter with complete debug info instead.

A small plugin looks like this:

```lua
maki.api.register_command({
  name = "greet",
  description = "Say hello from Lua",
  handler = function()
    maki.ui.flash("hello from a plugin!")
  end,
})
```

## How to read this reference

Signatures use Neovim notation: `{path}` is a required argument, `{opts?}`
is optional, and `{...}` is variadic.

One convention to remember: fallible runtime operations return a
`(value, err)` pair instead of throwing. Check `err` before using `value`:

```lua
local text, err = maki.fs.read("config.json")
if err then
  maki.log.error("read failed: " .. err)
  return
end
```

Lua errors are reserved for programmer mistakes, like passing a number where
a string belongs.

{PERMISSIONS}
"#;

const COMPACT_HEADER: &str = r#"# Lua API

Every module, function, and method, generated from source.

The API mirrors Neovim where possible (`maki.fs`, `maki.uv`, `maki.treesitter`,
`maki.keymap`, `maki.base64`); signatures are identical so code can be
copy-pasted between the two.

Signatures use Neovim notation: `{path}` is required, `{opts?}` is optional,
`{...}` is variadic. Lua errors are reserved for programmer mistakes, like
passing a number where a string belongs.
"#;

const HELPERS_INTRO: &str = "## Shared helper modules\n\nThese ship inside maki; `require` them from any plugin. Small modules are\nshown as full source, larger ones as their public interface.\n\n";

const FULL_SOURCE_MAX_BYTES: usize = 1024;

/// One guide for both readers, so the skill and the website cannot drift.
/// The caller fills the two slots that differ: permission rules spelled out
/// for the skill and linked for the site, and the agent-only notes.
fn guide(permissions: &str, agent_notes: &str) -> String {
    let body = GUIDE
        .replace("{PERMISSIONS}", permissions)
        .replace("{AGENT_NOTES}", agent_notes);
    format!("{body}{EXAMPLE}```\n")
}

/// The skill carries the guide, example, and a line-numbered index into
/// {reference}; the skill plugin writes the full reference to disk so the
/// model reads only the sections it needs.
fn skill_content(reference: &str) -> String {
    format!(
        "{guide}\n# Full API reference\n\n\
         The complete Lua API reference - every function with parameters, return\n\
         values, and examples, plus shared helper module sources - is on disk at:\n\n\
         `{REFERENCE_PLACEHOLDER}`\n\n\
         The index below maps every function to its line in that file. Before using\n\
         a function you are not certain about, read its section (read tool with\n\
         offset = line number) or grep the file for its name. Never guess a\n\
         signature or parameter table from the index alone.\n\n\
         Signatures use Neovim notation: `{{path}}` is required, `{{opts?}}` is\n\
         optional, `{{...}}` is variadic.\n{}",
        reference_index(reference),
        guide = guide(permissions_section(false).trim_end(), AGENT_NOTES),
    )
}

/// Rust-backed `require()` modules: the skill plugin loads the builtin
/// `maki-plugin-dev` skill from here instead of generated files on disk.
/// Rendering repeats per runtime, but Lua's `loaded` table caches the result
/// and the cost is a one-shot string build at plugin load.
pub(crate) fn virtual_module(lua: &Lua, modname: &str) -> Option<mlua::Result<Table>> {
    if modname != SKILL_MODULE && modname != REFERENCE_MODULE {
        return None;
    }
    let build = || {
        let table = lua.create_table()?;
        if modname == SKILL_MODULE {
            table.set("name", NAME)?;
            table.set("description", DESCRIPTION)?;
            table.set("reference_placeholder", REFERENCE_PLACEHOLDER)?;
            table.set("content", skill_content(&reference()))?;
        } else {
            table.set("content", reference())?;
        }
        Ok(table)
    };
    Some(build())
}

/// The body of the website's "Lua API" page: full render with anchors and
/// an overview table, plus the shared helper modules.
pub fn site_page() -> String {
    format!("{}\n{}", render(false), helpers_section())
}

/// The body of the website's "Plugins" page: the guide the skill hands the
/// model, with the permission rules linked instead of copied.
pub fn guide_page() -> String {
    let permissions = format!(
        "## Permissions and plugin.toml\n\n\
         Sensitive APIs are gated per plugin file, and a plugin without a\n\
         `plugin.toml` next to it gets nothing. The gates and the file format are\n\
         in [the reference]({REFERENCE_URL}#{PERMISSIONS_ANCHOR}). Set\n\
         `min_maki_version` there when a plugin needs a newer Maki Lua API."
    );
    format!(
        "{}\n## Full API reference\n\n\
         Every module, function, and method is in the [Lua API reference]({REFERENCE_URL}).\n\
         The agent gets the same document on disk through the builtin\n\
         `maki-plugin-dev` skill, so asking it to write a plugin for you works\n\
         without pasting any of this.\n",
        guide(&permissions, ""),
    )
}

/// The full API reference plus the shared helper modules: the exact document
/// the skill plugin writes to disk.
fn reference() -> String {
    format!("{}\n---\n\n{}", render(true), helpers_section())
}

/// Index of {reference}: every `##`/`###` heading with its 1-based line
/// number, plus a first-sentence summary for functions and helper modules.
fn reference_index(reference: &str) -> String {
    let lines: Vec<&str> = reference.lines().collect();
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some(sig) = line.strip_prefix("### ") {
            let summary = index_summary(&lines[i + 1..]);
            out.push_str(&format!("- L{} {sig}{summary}\n", i + 1));
        } else if let Some(module) = line.strip_prefix("## ") {
            out.push_str(&format!("\n## {module} - L{}\n", i + 1));
        }
    }
    out
}

fn index_summary(rest: &[&str]) -> String {
    let mut it = rest.iter().map(|l| l.trim()).filter(|l| !l.is_empty());
    let first = match it.next() {
        Some(l) if l.starts_with("```") => match it.next().and_then(|l| l.strip_prefix("-- ")) {
            Some(comment) => comment,
            None => return String::new(),
        },
        Some(l) if !l.starts_with('#') && !l.starts_with("**") => l,
        _ => return String::new(),
    };
    format!(" - {}", first_sentence(first))
}

fn is_public_fn(line: &str) -> bool {
    line.strip_prefix("function ")
        .and_then(|rest| rest.split_once(['.', ':']))
        .and_then(|(_, method)| method.chars().next())
        .is_some_and(|c| c != '_')
}

fn is_export(line: &str) -> bool {
    let Some((lhs, rhs)) = line.split_once(" = ") else {
        return false;
    };
    if rhs.is_empty() || rhs.ends_with('{') {
        return false;
    }
    let Some((table, field)) = lhs.split_once('.') else {
        return false;
    };
    let ident = |s: &str| {
        let mut chars = s.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    ident(table) && ident(field)
}

fn skeleton(src: &str) -> String {
    fn flush(out: &mut String, pending: &mut Vec<&str>) {
        for line in pending.drain(..) {
            out.push_str(line);
            out.push('\n');
        }
    }
    let mut out = String::new();
    let mut pending: Vec<&str> = Vec::new();
    let mut leading = true;
    for line in src.lines() {
        if line.starts_with("--") {
            pending.push(line);
            continue;
        }
        if leading {
            flush(&mut out, &mut pending);
            leading = false;
        }
        if is_public_fn(line) || is_export(line) {
            if !pending.is_empty() && !out.is_empty() {
                out.push('\n');
            }
            flush(&mut out, &mut pending);
            out.push_str(line);
            out.push('\n');
        } else {
            pending.clear();
        }
    }
    out
}

fn helpers() -> Vec<(String, &'static str)> {
    let maki = lib_dir()
        .get_dir("maki")
        .expect("plugins/lib/maki embedded");
    let mut helpers: Vec<(String, &'static str)> = maki
        .files()
        .filter_map(|file| {
            let path = file.path();
            let stem = path.file_stem()?.to_str()?;
            (path.extension()? == "lua")
                .then(|| (format!("maki.{stem}"), file.contents_utf8().unwrap()))
        })
        .collect();
    helpers.sort();
    helpers
}

fn helpers_section() -> String {
    let mut out = String::from(HELPERS_INTRO);
    for (name, src) in helpers() {
        let body = if src.len() <= FULL_SOURCE_MAX_BYTES {
            src.to_owned()
        } else {
            skeleton(src)
        };
        out.push_str(&format!(
            "### `require(\"{name}\")`\n\n```lua\n{body}```\n\n"
        ));
    }
    out
}

fn slug(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

fn instance_name(module: &ModuleDoc) -> &'static str {
    module.name.rsplit('.').next().unwrap_or(module.name)
}

fn first_sentence(desc: &str) -> &str {
    let first_line = desc.lines().next().unwrap_or_default();
    match first_line.find(". ") {
        Some(i) => &first_line[..=i],
        None => first_line,
    }
}

type ClassLinks = Vec<(&'static str, String)>;

fn class_links() -> ClassLinks {
    let mut links = ClassLinks::new();
    for module in api_docs() {
        if module.kind == DocKind::Class && !links.iter().any(|(n, _)| *n == module.name) {
            let id = slug(module.name);
            links.push((module.name, id.clone()));
            links.push((instance_name(module), id));
        }
    }
    links
}

fn link_ty(ty: &str, classes: &ClassLinks) -> String {
    let base = ty
        .trim_end_matches("|nil")
        .trim_end_matches('?')
        .trim_end_matches("[]");
    match classes.iter().find(|(name, _)| *name == base) {
        Some((_, id)) => format!("[`{ty}`](#{id})"),
        None => format!("`{ty}`"),
    }
}

fn format_returns(returns: &str, classes: &ClassLinks) -> String {
    let Some((types, desc)) = returns
        .strip_prefix('(')
        .and_then(|rest| rest.split_once(')'))
    else {
        return returns.to_owned();
    };
    let types = types
        .split(", ")
        .map(|ty| link_ty(ty, classes))
        .collect::<Vec<_>>()
        .join(", ");
    format!("({types}){desc}")
}

fn field_item(text: &str) -> Option<String> {
    let rest = text.strip_prefix("- ").unwrap_or(text);
    let (name, rest) = match rest.strip_prefix('`') {
        Some(r) => r.split_once('`')?,
        None => {
            let end = rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '_')?;
            if end == 0 {
                return None;
            }
            rest.split_at(end)
        }
    };
    let (ty, desc) = rest
        .strip_prefix(' ')?
        .trim_start()
        .strip_prefix('(')?
        .split_once(')')?;
    if ty.is_empty() || ty.contains('(') {
        return None;
    }
    let desc = match desc.chars().next() {
        None => "",
        Some(' ') => {
            let d = desc.trim_start();
            d.strip_prefix("- ").map_or(d, str::trim_start)
        }
        Some(':') => desc[1..].trim_start(),
        _ => return None,
    };
    Some(format!("`{name}` (`{ty}`) {desc}"))
}

fn push_fields_block(out: &mut String, block: &str) {
    let mut levels: Vec<usize> = Vec::new();
    for raw in block.lines() {
        let line = raw.strip_prefix("  ").unwrap_or(raw);
        let text = line.trim_start();
        if text.is_empty() {
            continue;
        }
        let indent = line.len() - text.len();
        if let Some(item) = field_item(text) {
            while levels.last().is_some_and(|&i| i > indent) {
                levels.pop();
            }
            if levels.last() != Some(&indent) {
                levels.push(indent);
            }
            out.push_str(&format!("{}- {item}\n", "  ".repeat(levels.len())));
        } else if levels.last().is_some_and(|&i| indent > i) {
            out.push_str(&format!("{}{text}\n", "  ".repeat(levels.len() + 1)));
        } else {
            levels.clear();
            out.push_str(&format!("\n  {text}\n\n"));
        }
    }
}

fn push_fn(out: &mut String, module: &ModuleDoc, f: &FnDoc, classes: &ClassLinks, compact: bool) {
    let (owner, sep) = match module.kind {
        DocKind::Table => (module.name, '.'),
        DocKind::Class => (instance_name(module), ':'),
    };
    let sig = format!("{owner}{sep}{}({})", f.name, f.args);
    if compact {
        out.push_str(&format!("### `{sig}`\n\n"));
    } else {
        let title = format!("{owner}{sep}{}()", f.name);
        let id = slug(&title);
        out.push_str(&format!(
            "### `{title}` {{#{id}}}\n\n```lua\n{sig}\n```\n\n"
        ));
    }
    if !f.desc.is_empty() {
        out.push_str(f.desc);
        out.push_str("\n\n");
    }
    if let Some(guard) = f.guard {
        if compact {
            out.push_str(&format!("Requires the `{guard}` plugin permission.\n\n"));
        } else {
            out.push_str(&format!(
                "Requires the `{guard}` [plugin permission](#{PERMISSIONS_ANCHOR}).\n\n"
            ));
        }
    }
    if !f.params.is_empty() {
        out.push_str("**Parameters:**\n\n");
        for p in f.params {
            let (first, rest) = p.desc.split_once('\n').unwrap_or((p.desc, ""));
            out.push_str(&format!(
                "- `{}` ({}) {first}\n",
                p.name,
                link_ty(p.ty, classes)
            ));
            push_fields_block(out, rest);
        }
        out.push('\n');
    }
    if !f.returns.is_empty() {
        out.push_str(&format!(
            "**Returns:** {}\n\n",
            format_returns(f.returns, classes)
        ));
    }
    if !f.example.is_empty() {
        out.push_str(&format!("**Example:**\n\n```lua\n{}\n```\n\n", f.example));
    }
}

fn render(compact: bool) -> String {
    let mut merged: Vec<(&str, Vec<&'static ModuleDoc>)> = Vec::new();
    for module in api_docs() {
        match merged.iter_mut().find(|(name, _)| *name == module.name) {
            Some((_, modules)) => modules.push(module),
            None => merged.push((module.name, vec![module])),
        }
    }

    let classes = if compact {
        ClassLinks::new()
    } else {
        class_links()
    };
    let mut out = if compact {
        String::from(COMPACT_HEADER)
    } else {
        HEADER.replace("{PERMISSIONS}", permissions_section(true).trim_end())
    };

    if !compact {
        out.push_str("\n## Overview\n\n| Module | What it is for |\n| --- | --- |\n");
        for (name, modules) in &merged {
            let desc = modules
                .iter()
                .map(|m| first_sentence(m.desc))
                .find(|d| !d.is_empty())
                .unwrap_or_default();
            out.push_str(&format!("| [`{name}`](#{}) | {desc} |\n", slug(name)));
        }
    }

    for (name, modules) in merged {
        if compact {
            out.push_str(&format!("\n## {name}\n\n"));
        } else {
            out.push_str(&format!("\n## {name} {{#{}}}\n\n", slug(name)));
        }
        for module in &modules {
            if !module.desc.is_empty() {
                out.push_str(module.desc);
                out.push_str("\n\n");
            }
        }
        for module in &modules {
            for f in module.fns {
                if !compact {
                    out.push_str("---\n\n");
                }
                push_fn(&mut out, module, f, &classes, compact);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        AGENT_NOTES, PERMISSIONS_ANCHOR, REFERENCE_URL, guide_page, reference, reference_index,
        site_page, skeleton, skill_content,
    };

    const AGENT_VOICE: &str = "ask the user";

    const MODULE: &str = "-- Header line one.\n-- Header line two.\nlocal M = {}\nM.__index = M\nM.CONST = \"x\"\nM.specs = {\n  a = 1,\n}\nlocal function private()\nend\n-- Doc for pub.\nfunction M.pub(a, b)\n  local inner = 1\nend\nfunction M:_hidden()\nend\nfunction M:method()\nend\nreturn M\n";

    #[test]
    fn skeleton_keeps_public_surface_only() {
        let expected = "-- Header line one.\n-- Header line two.\nM.CONST = \"x\"\n\n-- Doc for pub.\nfunction M.pub(a, b)\nfunction M:method()\n";
        assert_eq!(skeleton(MODULE), expected);
    }

    #[test]
    fn reference_index_lists_headings_with_line_numbers() {
        let reference = "# Lua API\n\n## maki.api\n\nModule desc.\n\n\
            ### `maki.api.register_tool({spec})`\n\nRegister a tool. More text.\n\n\
            ### `maki.api.bare()`\n\n**Parameters:**\n\n\
            ## Shared helper modules\n\n### `require(\"maki.color\")`\n\n\
            ```lua\n-- Terminal colors helper.\nlocal M = {}\n```\n";
        let expected = "\n## maki.api - L3\n\
            - L7 `maki.api.register_tool({spec})` - Register a tool.\n\
            - L11 `maki.api.bare()`\n\n\
            ## Shared helper modules - L15\n\
            - L17 `require(\"maki.color\")` - Terminal colors helper.\n";
        assert_eq!(reference_index(reference), expected);
    }

    /// The website reader is not the agent, so nothing addressed to the agent
    /// may leak out of [`AGENT_NOTES`] into the shared guide.
    #[test]
    fn only_the_skill_speaks_to_the_agent() {
        let skill = skill_content("");
        assert!(skill.contains(AGENT_NOTES.trim_end()));
        assert!(
            skill.contains(AGENT_VOICE),
            "the notes must say \"{AGENT_VOICE}\", or the check below proves nothing"
        );
        assert!(
            !guide_page().contains(AGENT_VOICE),
            "website guide should never say \"{AGENT_VOICE}\""
        );
    }

    #[test]
    fn guide_permission_link_has_an_anchor_in_the_reference() {
        let link = format!("{REFERENCE_URL}#{PERMISSIONS_ANCHOR}");
        assert!(guide_page().contains(&link), "guide should link {link}");
        assert!(
            site_page().contains(&format!("{{#{PERMISSIONS_ANCHOR}}}")),
            "lua api page should carry the {PERMISSIONS_ANCHOR} anchor for {link}"
        );
    }

    #[test]
    fn plugin_dev_index_points_at_reference_headings() {
        let reference = reference();
        let content = skill_content(&reference);
        assert!(content.contains(super::REFERENCE_PLACEHOLDER));
        let lines: Vec<&str> = reference.lines().collect();
        let mut checked = 0;
        for line in content.lines() {
            let Some((num, rest)) = line
                .strip_prefix("- L")
                .and_then(|rest| rest.split_once(" `"))
            else {
                continue;
            };
            let (Ok(num), Some((sig, _))) = (num.parse::<usize>(), rest.split_once('`')) else {
                continue;
            };
            let target = lines[num - 1];
            assert!(
                target.starts_with(&format!("### `{sig}`")),
                "index L{num} should point at ### `{sig}`, got: {target}"
            );
            checked += 1;
        }
        assert!(
            checked > 100,
            "index should cover the reference, checked {checked}"
        );
    }
}
