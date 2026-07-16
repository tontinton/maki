use maki_lua::{DocKind, FnDoc, ModuleDoc, api_docs};

const HEADER: &str = r#"+++
title = "Lua API"
weight = 6
[extra]
group = "Reference"
+++

# Lua API

Maki plugins are plain Lua files. Everything a plugin can touch lives under
one global table: `maki`. This page documents every module, function, and
method. It is generated straight from the source code by `maki-docgen`, so it
can never drift from what maki actually does.

If you have written Neovim plugins before, you are already halfway there.
Where a module mirrors Neovim (`maki.fs`, `maki.uv`, `maki.treesitter`,
`maki.keymap`, `maki.base64`), signatures are kept identical so code can be
copy-pasted between the two.

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

## How to read this page

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

fn push_fn(out: &mut String, module: &ModuleDoc, f: &FnDoc, classes: &ClassLinks) {
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
    let id = slug(&title);
    out.push_str(&format!(
        "### `{title}` {{#{id}}}\n\n```lua\n{sig}\n```\n\n"
    ));
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

    let classes = class_links();
    let mut out = String::from(HEADER);

    out.push_str("\n## Overview\n\n| Module | What it is for |\n| --- | --- |\n");
    for (name, modules) in &merged {
        let desc = modules
            .iter()
            .map(|m| first_sentence(m.desc))
            .find(|d| !d.is_empty())
            .unwrap_or_default();
        out.push_str(&format!("| [`{name}`](#{}) | {desc} |\n", slug(name)));
    }

    for (name, modules) in merged {
        out.push_str(&format!("\n## {name} {{#{}}}\n\n", slug(name)));
        for module in &modules {
            if !module.desc.is_empty() {
                out.push_str(module.desc);
                out.push_str("\n\n");
            }
        }
        for module in &modules {
            for f in module.fns {
                out.push_str("---\n\n");
                push_fn(&mut out, module, f, &classes);
            }
        }
    }
    out
}
