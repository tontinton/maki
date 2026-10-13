+++
title = "Keybindings"
weight = 9
[extra]
group = "Reference"
+++

# Keybindings

On macOS, some bindings use Option or Fn keys instead (run `/help` for exact keybindings).

## General

| Key | Action |
|-----|--------|
| `Ctrl+C` | Quit / clear input |
| `Ctrl+H` | Show keybindings |
| `Ctrl+F` | Search messages |
| `Ctrl+S` | File picker |
| `Ctrl+O` | Open plan in editor |
| `Ctrl+T` | Toggle plan panel |
| `Ctrl+L` | Model picker |

## Editing

| Key | Action |
|-----|--------|
| `Enter` | Submit prompt |
| `Shift+Enter` / `Ctrl+Enter` / `Ctrl+J` / `Alt+Enter` | Newline |
| `Tab` | Switch between plan and build |
| `/command` | Open command palette |
| `Ctrl+W` | Delete word backward |
| `Alt+←` / `Alt+→` | Move word left / right |
| `Ctrl+A` | Jump to start of line |
| `Home` / `End` | Jump to start/end of line |
| `Ctrl+U` / `Ctrl+D` | Scroll half page up / down |
| `PageUp` / `PageDown` | Scroll page up / down |
| `Ctrl+E` | Jump to end of line |
| `Ctrl+G` | Scroll to top |
| `Ctrl+Alt+G` | Scroll to bottom |
| `Ctrl+Q` | Pop queue |
| `Esc Esc` | Rewind |
| `Alt+O` | Edit input in external editor |
| `Alt+Z` | Undo typing / paste |

### macOS-specific

| Key | Action |
|-----|--------|
| `Ctrl+Del` / `⌥Del` | Delete word forward |
| `Ctrl+K` | Delete to end of line |

## While Streaming

| Key | Action |
|-----|--------|
| `↑` / `↓` | Navigate input history |
| `Esc Esc` | Cancel agent |

## Form

| Key | Action |
|-----|--------|
| `↑` / `↓` | Navigate options |
| `Enter` | Select option |
| `Esc` | Close |

## Pickers

| Key | Action |
|-----|--------|
| `↑` / `↓` | Navigate |
| `Enter` | Select |
| `Esc` | Close |
| `Type` | Filter |
| `PageUp` / `PageDown` | Scroll page up / down |
| `Ctrl+U` / `Ctrl+D` | Scroll half page up / down |

## Context-Specific

Some pickers add extra bindings on top of the defaults:

| Context | Key | Action |
|---------|-----|--------|
| Rewind Picker | `Ctrl+F` | Fork up to the selected prompt and switch to it |
| Queue | `Enter` | Remove item |
| Commands | `Tab` | Complete command |
| Model Picker | `!/@/#/$` | Set tier (strong/medium/weak/compaction) |
| Model Picker | `%` | Fold or unfold provider |
| Session Picker | `Ctrl+N` | New session |
| Session Picker | `Ctrl+R` | Rename session |
| Session Picker | `Ctrl+D` | Delete session (press twice) |
| Thinking Picker | `↑`/`↓` | Move between effort levels |
| Thinking Picker | `0`-`9` | Type a token budget |
| Thinking Picker | `Enter` | Apply and close |
| Thinking Picker | `Esc` | Close without changing anything |
| Task Picker | `Ctrl+D` | Delete finished task (press twice) |

## Plugins

Built-in plugins register these themselves, and your own plugins can add more with `maki.keymap.set`:

| Key | Action |
|-----|--------|
| `Ctrl+P` | Browse sessions |
| `Ctrl+X` | Open tasks |
| `Alt+T` | Thinking effort |

## Context Inheritance

Child contexts inherit their parent's bindings and add their own.

- **Pickers** is the base for: Rewind Picker, Theme Picker, Model Picker, Queue, Commands, Search, File Picker

## Overriding Keybindings

Rebind a single key from your `init.lua` with [`maki.keymap.set`](/docs/lua-api/#maki-keymap-set). To move a built-in to another key, call its [action](/docs/lua-api/#maki-ui-action). This opens the file picker on `Alt+S`, for terminals that take `Ctrl+S`:

```lua
maki.keymap.set({ "n", "i" }, "<M-s>", function()
  maki.ui.action("file_picker")
end)
```

`{ "n", "i" }` lists both [modes](/docs/lua-api/#modes). Your binding replaces the default on that key, and the default comes back when the binding is removed. When two bindings share a key, the last `set` wins, and the log names both.

Some keys stay out of reach (the full order is in [Key resolution](/docs/lua-api/#key-resolution)):

- `Ctrl+C` and `Ctrl+Z`, so quit and suspend always work.
- `Esc` while the agent runs, so `Esc Esc` can cancel the turn.
- The keys of an open modal, picker or prompt, which it handles first.
- Keys a plugin binds while it loads, like `Ctrl+P`. Plugins load after your global and project `init.lua`, so their `set` comes last.
- Multi-key combinations and rows like `Type` to filter. Only single keys can be bound.

`/help` and the splash show the default labels, even for a key you rebound. Below them, `/help` lists every key that plugins and your `init.lua` bound, with its modes and owner, and every action you can call.

### Upgrading old keymaps

Older versions fired `"n"` bindings while you typed in the chat input. Now they fire only in normal mode, so a shortcut set in `"n"` alone stops working while you type. List both modes:

```lua
-- Before:
maki.keymap.set("n", "<M-n>", open_notes)
-- After:
maki.keymap.set({ "n", "i" }, "<M-n>", open_notes)
```

At startup, the status bar warns about a plugin or `init.lua` that binds a key in `"n"` alone. One that also binds a key in `"i"`, like a vim mode, handles modes itself and gets no warning.

### Composing keys

`Tab` switches between plan and build. To make it accept a file completion first while the completion menu is open, bind it in your `init.lua` and try each [action](/docs/lua-api/#maki-ui-action) in turn. `ev.fallback()` hands the key back to Maki when none of them took it:

```lua
maki.keymap.set("i", "<Tab>", function(ev)
  if maki.ui.action("completion.accept") then return end
  if maki.ui.action("ghost.accept") then return end
  ev.fallback()
end)
```

`ghost.accept` stands for another plugin's action. An action returns `true` when it handled the key. If its plugin is not installed or is disabled, it returns `nil` and the binding moves on.

`completion.accept` writes file paths into the input, so calling it needs `fs_read`, `input` and `session_read` in `~/.config/maki/plugin.toml` (see [plugin permissions](/docs/lua-api/#plugin-permissions)):

```toml
[permissions]
fs_read = true
input = true
session_read = true
```

Without them the action returns `nil`, `Tab` switches between plan and build, and the status bar names the missing permission.

### Recovering from a bad keymap

If an override leaves Maki stuck (a modal that does not close, a plugin that throws on load), boot without user `init.lua`:

```bash
maki --no-plugins
```

Skips user `init.lua` files (global and project) but keeps the Lua host and builtin plugins running, so tools still work. Custom commands and skills still load, and the project permission rules and env file follow [folder trust](/docs/folder-trust/).

The default keymap lives in Rust, not Lua, so `--no-plugins` never drops it.

## Shell and images

These are input conventions, not remappable key rows:

- Prefix a line with `!` to run a shell command yourself (5 minute timeout). Use `!!` to hide the command and its output from the agent.
- `Ctrl+V` pastes an image from the clipboard into the prompt when the model supports vision. You can also paste image file paths.
- Middle-click inserts the PRIMARY selection, the same text middle-click pastes in a shell. Linux only, since other platforms have no PRIMARY selection. On Wayland the compositor must support it.
