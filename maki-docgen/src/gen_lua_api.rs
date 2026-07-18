use std::fs;

use maki_lua::{DocKind, FnDoc, ModuleDoc, api_docs};

const FRONTMATTER: &str = r#"+++
title = "Lua API"
weight = 6
[extra]
group = "Reference"
+++

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

const HELPERS_DIR: &str = "plugins/lib/maki";

const FULL_SOURCE_MAX_BYTES: usize = 1024;

const HELPERS_INTRO: &str = "## Shared helper modules\n\nThese ship inside maki; `require` them from any plugin. Small modules are\nshown as full source, larger ones as their public interface.\n\n";

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

fn helpers() -> Vec<(String, String)> {
    let entries = fs::read_dir(HELPERS_DIR)
        .unwrap_or_else(|e| panic!("read {HELPERS_DIR} (run from repo root): {e}"));
    let mut helpers: Vec<(String, String)> = entries
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            let stem = path.file_stem()?.to_str()?.to_owned();
            (path.extension()? == "lua").then(|| {
                let src = fs::read_to_string(&path).unwrap();
                (format!("maki.{stem}"), src)
            })
        })
        .collect();
    helpers.sort();
    helpers
}

fn helpers_section() -> String {
    let mut out = String::from(HELPERS_INTRO);
    for (name, src) in helpers() {
        let body = if src.len() <= FULL_SOURCE_MAX_BYTES {
            src
        } else {
            skeleton(&src)
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
    let (title, sig) = match module.kind {
        DocKind::Table => (
            format!("{}.{}()", module.name, f.name),
            format!("{}.{}({})", module.name, f.name, f.args),
        ),
        DocKind::Class => (
            format!("{}:{}()", instance_name(module), f.name),
            format!("{}:{}({})", instance_name(module), f.name, f.args),
        ),
    };
    if compact {
        out.push_str(&format!("### `{sig}`\n\n"));
    } else {
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
            match p.desc.split_once('\n') {
                Some((first, rest)) => {
                    out.push_str(&format!(
                        "- `{}` ({}) {first}\n",
                        p.name,
                        link_ty(p.ty, classes)
                    ));
                    push_fields_block(out, rest);
                }
                None => out.push_str(&format!(
                    "- `{}` ({}) {}\n",
                    p.name,
                    link_ty(p.ty, classes),
                    p.desc
                )),
            }
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

pub fn generate() -> String {
    format!("{FRONTMATTER}{}\n{}", render(false), helpers_section())
}

/// The full API reference plus the shared helper modules: the exact document
/// the skill plugin writes to disk.
pub fn reference() -> String {
    format!("{}\n---\n\n{}", render(true), helpers_section())
}

/// Index of {reference}: every `##`/`###` heading with its 1-based line
/// number, plus a first-sentence summary for functions and helper modules.
pub fn reference_index(reference: &str) -> String {
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

fn render(compact: bool) -> String {
    let mut order: Vec<&str> = Vec::new();
    let mut merged: Vec<(&str, Vec<&'static ModuleDoc>)> = Vec::new();
    for module in api_docs() {
        match order.iter().position(|n| *n == module.name) {
            Some(i) => merged[i].1.push(module),
            None => {
                order.push(module.name);
                merged.push((module.name, vec![module]));
            }
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
    use super::{reference_index, skeleton};

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
}
