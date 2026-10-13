use maki_ui::keybindings::{ALT_SEP, KEYBINDS, KeyLabel, KeybindContext, Platform, all_contexts};

const FRONTMATTER: &str = "\
+++
title = \"Keybindings\"
weight = 9
[extra]
group = \"Reference\"
+++";

const LUA_CONTEXT_BINDS: &[(&str, &str, &str)] = &[
    ("Session Picker", "`Ctrl+N`", "New session"),
    ("Session Picker", "`Ctrl+R`", "Rename session"),
    ("Session Picker", "`Ctrl+D`", "Delete session (press twice)"),
    ("Thinking Picker", "`↑`/`↓`", "Move between effort levels"),
    ("Thinking Picker", "`0`-`9`", "Type a token budget"),
    ("Thinking Picker", "`Enter`", "Apply and close"),
    (
        "Thinking Picker",
        "`Esc`",
        "Close without changing anything",
    ),
    (
        "Task Picker",
        "`Ctrl+D`",
        "Delete finished task (press twice)",
    ),
];

// Built-in plugins own these globally, so they never reach `KEYBINDS`.
const PLUGIN_BINDS: &[(&str, &str)] = &[
    ("`Ctrl+P`", "Browse sessions"),
    ("`Ctrl+X`", "Open tasks"),
    ("`Alt+T`", "Thinking effort"),
];

const MAIN_CONTEXTS: &[KeybindContext] = &[
    KeybindContext::General,
    KeybindContext::Editing,
    KeybindContext::Streaming,
    KeybindContext::FormInput,
    KeybindContext::Picker,
];

fn label_str(label: KeyLabel) -> String {
    match label {
        KeyLabel::Single(s) => format!("`{s}`"),
        KeyLabel::Alt(a, b) => format!("`{a}`{ALT_SEP}`{b}`"),
        KeyLabel::MacAlt(a, _) => format!("`{a}`"),
        KeyLabel::MacMulti(normal, _) => normal
            .iter()
            .map(|s| format!("`{s}`"))
            .collect::<Vec<_>>()
            .join(ALT_SEP),
    }
}

fn write_table_2col(out: &mut String, rows: &[(String, &str)]) {
    out.push_str("| Key | Action |\n|-----|--------|\n");
    for (key, desc) in rows {
        out.push_str(&format!("| {key} | {desc} |\n"));
    }
}

fn write_section(out: &mut String, ctx: KeybindContext) {
    out.push_str(&format!("\n## {}\n\n", ctx.label()));

    let all_rows: Vec<_> = KEYBINDS.iter().filter(|kb| kb.context == ctx).collect();

    let normal: Vec<_> = all_rows
        .iter()
        .filter(|kb| kb.platform == Platform::All)
        .map(|kb| (label_str(kb.label), kb.description))
        .collect();

    if !normal.is_empty() {
        write_table_2col(out, &normal);
    }

    let mac_only: Vec<_> = all_rows
        .iter()
        .filter(|kb| kb.platform == Platform::MacOnly)
        .map(|kb| (label_str(kb.label), kb.description))
        .collect();

    if !mac_only.is_empty() {
        out.push_str("\n### macOS-specific\n\n");
        write_table_2col(out, &mac_only);
    }
}

fn write_context_specific(out: &mut String) {
    let child_binds: Vec<_> = KEYBINDS
        .iter()
        .filter(|kb| kb.context.parent().is_some())
        .collect();

    if child_binds.is_empty() {
        return;
    }

    out.push_str("\n## Context-Specific\n\n");
    out.push_str("Some pickers add extra bindings on top of the defaults:\n\n");
    out.push_str("| Context | Key | Action |\n|---------|-----|--------|\n");

    for kb in &child_binds {
        let key = label_str(kb.label);
        out.push_str(&format!(
            "| {} | {key} | {} |\n",
            kb.context.label(),
            kb.description
        ));
    }

    for (ctx, key, desc) in LUA_CONTEXT_BINDS {
        out.push_str(&format!("| {ctx} | {key} | {desc} |\n"));
    }
}

fn write_plugin_binds(out: &mut String) {
    out.push_str("\n## Plugins\n\n");
    out.push_str("Built-in plugins register these themselves, and your own plugins can add more with `maki.keymap.set`:\n\n");
    out.push_str("| Key | Action |\n|-----|--------|\n");
    for (key, desc) in PLUGIN_BINDS {
        out.push_str(&format!("| {key} | {desc} |\n"));
    }
}

fn write_inheritance(out: &mut String) {
    let children: Vec<_> = all_contexts()
        .filter(|ctx| ctx.parent().is_some())
        .collect();

    if children.is_empty() {
        return;
    }

    out.push_str("\n## Context Inheritance\n\n");
    out.push_str("Child contexts inherit their parent's bindings and add their own.\n\n");

    let mut by_parent: Vec<(KeybindContext, Vec<&str>)> = Vec::new();
    for child in &children {
        let parent = child.parent().unwrap();
        if let Some(entry) = by_parent.iter_mut().find(|(p, _)| *p == parent) {
            entry.1.push(child.label());
        } else {
            by_parent.push((parent, vec![child.label()]));
        }
    }

    for (parent, kids) in &by_parent {
        let list = kids.join(", ");
        out.push_str(&format!(
            "- **{}** is the base for: {list}\n",
            parent.label()
        ));
    }
}

pub fn generate() -> String {
    let mut out = String::from(FRONTMATTER);
    out.push_str("\n\n# Keybindings\n\n");
    out.push_str("On macOS, some bindings use Option or Fn keys instead (run `/help` for exact keybindings).\n");

    for &ctx in MAIN_CONTEXTS {
        write_section(&mut out, ctx);
    }

    write_context_specific(&mut out);
    write_plugin_binds(&mut out);
    write_inheritance(&mut out);
    write_overrides(&mut out);

    out
}

fn write_overrides(out: &mut String) {
    out.push_str("\n## Overriding Keybindings\n\n");
    out.push_str(
        "Rebind a single key from your `init.lua` with \
         [`maki.keymap.set`](/docs/lua-api/#maki-keymap-set). To move a \
         built-in to another key, call its \
         [action](/docs/lua-api/#maki-ui-action). This opens the file \
         picker on `Alt+S`, for terminals that take `Ctrl+S`:\n\n",
    );
    out.push_str(
        "```lua\nmaki.keymap.set({ \"n\", \"i\" }, \"<M-s>\", function()\n  \
         maki.ui.action(\"file_picker\")\nend)\n```\n\n",
    );
    out.push_str(
        "`{ \"n\", \"i\" }` lists both [modes](/docs/lua-api/#modes). Your \
         binding replaces the default on that key, and the default comes \
         back when the binding is removed. When two bindings share a key, \
         the last `set` wins, and the log names both.\n\n",
    );
    out.push_str(
        "Some keys stay out of reach (the full order is in \
         [Key resolution](/docs/lua-api/#key-resolution)):\n\n\
         - `Ctrl+C` and `Ctrl+Z`, so quit and suspend always work.\n\
         - `Esc` while the agent runs, so `Esc Esc` can cancel the turn.\n\
         - The keys of an open modal, picker or prompt, which it handles \
         first.\n\
         - Keys a plugin binds while it loads, like `Ctrl+P`. Plugins load \
         after your global and project `init.lua`, so their `set` comes \
         last.\n\
         - Multi-key combinations and rows like `Type` to filter. Only \
         single keys can be bound.\n\n",
    );
    out.push_str(
        "`/help` and the splash show the default labels, even for a key you \
         rebound. Below them, `/help` lists every key that plugins and your \
         `init.lua` bound, with its modes and owner, and every action you \
         can call.\n\n",
    );
    out.push_str("### Upgrading old keymaps\n\n");
    out.push_str(
        "Older versions fired `\"n\"` bindings while you typed in the chat \
         input. Now they fire only in normal mode, so a shortcut set in \
         `\"n\"` alone stops working while you type. List both modes:\n\n",
    );
    out.push_str(
        "```lua\n-- Before:\nmaki.keymap.set(\"n\", \"<M-n>\", open_notes)\n\
         -- After:\nmaki.keymap.set({ \"n\", \"i\" }, \"<M-n>\", open_notes)\n```\n\n",
    );
    out.push_str(
        "At startup, the status bar warns about a plugin or `init.lua` that \
         binds a key in `\"n\"` alone. One that also binds a key in `\"i\"`, \
         like a vim mode, handles modes itself and gets no warning.\n\n",
    );
    out.push_str("### Composing keys\n\n");
    out.push_str(
        "`Tab` switches between plan and build. To make it accept a file \
         completion first while the completion menu is open, bind it in your \
         `init.lua` and try each [action](/docs/lua-api/#maki-ui-action) in \
         turn. `ev.fallback()` hands the key back to Maki when none of them \
         took it:\n\n",
    );
    out.push_str(
        "```lua\nmaki.keymap.set(\"i\", \"<Tab>\", function(ev)\n  \
         if maki.ui.action(\"completion.accept\") then return end\n  \
         if maki.ui.action(\"ghost.accept\") then return end\n  \
         ev.fallback()\nend)\n```\n\n",
    );
    out.push_str(
        "`ghost.accept` stands for another plugin's action. An action returns \
         `true` when it handled the key. If its plugin is not installed or \
         is disabled, it returns `nil` and the binding moves on.\n\n",
    );
    out.push_str(
        "`completion.accept` writes file paths into the input, so calling it \
         needs `fs_read`, `input` and `session_read` in \
         `~/.config/maki/plugin.toml` (see \
         [plugin permissions](/docs/lua-api/#plugin-permissions)):\n\n",
    );
    out.push_str(
        "```toml\n[permissions]\nfs_read = true\ninput = true\nsession_read = true\n```\n\n",
    );
    out.push_str(
        "Without them the action returns `nil`, `Tab` switches between plan and \
         build, and the status bar names the missing permission.\n\n",
    );
    out.push_str("### Recovering from a bad keymap\n\n");
    out.push_str(
        "If an override leaves Maki stuck (a modal that does not close, a \
         plugin that throws on load), boot without user `init.lua`:\n\n",
    );
    out.push_str("```bash\nmaki --no-plugins\n```\n\n");
    out.push_str(
        "Skips user `init.lua` files (global and project) but keeps the \
         Lua host and builtin plugins running, so tools still work. \
         Custom commands and skills still load, and the project \
         permission rules and env file follow \
         [folder trust](/docs/folder-trust/).\n\n",
    );
    out.push_str(
        "The default keymap lives in Rust, not Lua, so `--no-plugins` \
         never drops it.\n\n",
    );
    out.push_str("## Shell and images\n\n");
    out.push_str(
        "These are input conventions, not remappable key rows:\n\n\
         - Prefix a line with `!` to run a shell command yourself (5 minute \
         timeout). Use `!!` to hide the command and its output from the agent.\n\
         - `Ctrl+V` pastes an image from the clipboard into the prompt when the \
         model supports vision. You can also paste image file paths.\n\
         - Middle-click inserts the PRIMARY selection, the same text \
         middle-click pastes in a shell. Linux only, since other platforms \
         have no PRIMARY selection. On Wayland the compositor must support it.\n",
    );
}
