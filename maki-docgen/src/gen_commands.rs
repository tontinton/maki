use std::fmt::Write;

use maki_ui::BUILTIN_COMMANDS;

use crate::lua_util;

pub fn generate() -> String {
    let mut out = String::new();
    writeln!(out, "+++").unwrap();
    writeln!(out, "title = \"Commands\"").unwrap();
    writeln!(out, "weight = 5").unwrap();
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
        writeln!(out, "| `{}` | {} |", cmd.name, cmd.description).unwrap();
    }
    for cmd in &lua_util::load_builtin_plugin_commands() {
        writeln!(out, "| `{}` | {} |", cmd.name, cmd.description).unwrap();
    }

    writeln!(out).unwrap();
    writeln!(out, "## Custom commands").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "You can define your own slash commands as Markdown files."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### Project commands").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Place `.md` files in `.maki/commands/` in your project root."
    )
    .unwrap();
    writeln!(out, "They appear in the palette as `/project:<filename>`.").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### User commands").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "Place `.md` files in `~/.config/maki/commands/`.").unwrap();
    writeln!(out, "They appear in the palette as `/user:<filename>`.").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Project commands override user commands with the same name."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "`.claude/commands/` directories are also supported for compatibility."
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
        "Use `$ARGUMENTS` in the command body. It gets replaced with whatever you type after the command name."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "For example, `/project:review main.rs` replaces `$ARGUMENTS` with `main.rs`."
    )
    .unwrap();

    if out.ends_with('\n') {
        out.pop();
    }
    out
}
