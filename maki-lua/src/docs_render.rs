//! Renders the Lua API reference and the builtin `maki-plugin-dev` skill
//! straight from `api_docs()` at runtime, so no generated files live in the
//! repo. `maki-docgen` calls [`site_page`] for the website; the plugin
//! `require()` sandbox serves [`virtual_module`] to the skill plugin.

use mlua::{Lua, Table};

use crate::docs::{DocKind, FnDoc, ModuleDoc, api_docs};
use crate::loader::lib_dir;

const SKILL_MODULE: &str = "plugin_dev";
const REFERENCE_MODULE: &str = "plugin_dev_reference";

const NAME: &str = "maki-plugin-dev";
const DESCRIPTION: &str = "Write or modify maki plugins or init.lua config in Lua: custom tools, slash commands, keymaps, UI. Authoring guide, real example, indexed maki Lua API reference. Load before any maki plugin work.";
const REFERENCE_PLACEHOLDER: &str = "__MAKI_REFERENCE_PATH__";

const EXAMPLE: &str = include_str!("../../plugins/glob/init.lua");

const GUIDE: &str = r#"# Writing maki plugins

Maki plugins are plain Lua files (Luau) that run inside maki. A plugin can
register tools the LLM calls, slash commands, keymaps, prompt hints, and
custom UI. Everything lives under the global `maki` table. An index of the
full API reference is at the end of this document.

## Where plugin code goes

- `~/.config/maki/init.lua` - global, loaded for every project
- `<project>/.maki/init.lua` - project-local

Either file can call `maki.setup({ ... })` for configuration and register
tools or commands. There are no separate plugin directories yet.

## Development loop

You cannot run slash commands or restart maki. After editing, ask the user
to run `/reload` (rebuilds plugins and config in place). Until then your
changes are not live. If a backtrace comes out useless, suggest restarting
with `--no-jit`.

To debug, add `maki.log.info|warn|error(...)` calls. They write to `maki.log`
in the dir `maki.env.logs_dir()` returns (Linux: `~/.local/logs/maki/`).
Read that file yourself after the user reloads and reproduces.

## Conventions

- Fallible runtime calls return a `(value, err)` pair; check `err` before using `value`.
- Tool handlers report failures with `{ llm_output = "error: ...", is_error = true }`, not by raising.
- The model picks tools by reading `description`, so state precisely what the tool does and when to use it.
- Reusable helpers ship with maki; see "Shared helper modules" in the reference file.

## A complete real example

The bundled `glob` tool, verbatim: options registration, schema, header and
restore hooks, error handling, LLM output truncation, collapsible UI view:

```lua
"#;

const HEADER: &str = r#"# Lua API

Maki plugins are plain Lua files. Everything a plugin can touch lives under
one global table: `maki`. This reference documents every module, function,
and method. It is generated straight from the source code by `maki-docgen`.

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

/// The skill carries the guide, example, and a line-numbered index into
/// {reference}; the skill plugin writes the full reference to disk so the
/// model reads only the sections it needs.
fn skill_content(reference: &str) -> String {
    format!(
        "{GUIDE}{EXAMPLE}```\n\n# Full API reference\n\n\
         The complete Lua API reference - every function with parameters, return\n\
         values, and examples, plus shared helper module sources - is on disk at:\n\n\
         `{REFERENCE_PLACEHOLDER}`\n\n\
         The index below maps every function to its line in that file. Before using\n\
         a function you are not certain about, read its section (read tool with\n\
         offset = line number) or grep the file for its name. Never guess a\n\
         signature or parameter table from the index alone.\n\n\
         Signatures use Neovim notation: `{{path}}` is required, `{{opts?}}` is\n\
         optional, `{{...}}` is variadic.\n{}",
        reference_index(reference)
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
    let mut out = String::from(if compact { COMPACT_HEADER } else { HEADER });

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
    use super::{reference, reference_index, skeleton, skill_content};

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
