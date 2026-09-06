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
| `Ctrl+M` | Model picker |

## Editing

| Key | Action |
|-----|--------|
| `Enter` | Submit prompt |
| `Shift+Enter` / `Ctrl+Enter` / `Ctrl+J` / `Alt+Enter` | Newline |
| `Tab` | Toggle mode |
| `/command` | Open command palette |
| `Ctrl+W` | Delete word backward |
| `Alt+←` / `Alt+→` | Move word left / right |
| `Ctrl+A` | Jump to start of line |
| `Home` / `End` | Jump to start/end of line |
| `Ctrl+U` / `Ctrl+D` | Scroll half page up / down |
| `Ctrl+E` | Jump to end of line |
| `Ctrl+G` | Scroll to top |
| `Ctrl+B` | Scroll to bottom |
| `Ctrl+Q` | Pop queue |
| `Esc Esc` | Rewind |
| `Alt+O` | Edit input in external editor |

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
| `Ctrl+U` / `Ctrl+D` | Scroll page up / down |

## Context-Specific

Some pickers add extra bindings on top of the defaults:

| Context | Key | Action |
|---------|-----|--------|
| Queue | `Enter` | Remove item |
| Commands | `Tab` | Complete command |
| Model Picker | `!/@/#/$` | Set tier (strong/medium/weak/compaction) |
| Session Picker | `Ctrl+N` | New session |
| Session Picker | `Ctrl+R` | Rename session |
| Session Picker | `Ctrl+D` | Delete session (press twice) |
| Task Picker | `Ctrl+D` | Delete finished task (press twice) |

## Plugins

Built-in plugins register these themselves, and your own plugins can add more with `maki.keymap.set`:

| Key | Action |
|-----|--------|
| `Ctrl+P` | Browse sessions |
| `Ctrl+X` | Open tasks |

## Context Inheritance

Child contexts inherit their parent's bindings and add their own.

- **Pickers** is the base for: Rewind Picker, Theme Picker, Model Picker, Queue, Commands, Search, File Picker

## Overriding Keybindings

Plugins and `init.lua` can rebind keys at runtime with `maki.keymap.set` and `maki.keymap.del`. The tables above are the built-in defaults. An override on the same key wins, unless a modal or overlay is open (help, plan form, permission prompt).

Precedence, high to low:

1. **Suspend** (`Ctrl+Z`, Unix). Always wins, non-remappable.
2. **Modal and overlay keys.** An open modal or picker consumes its keys first, so they cannot be shadowed while open.
3. **Lua overrides** from `maki.keymap.set`. Last set wins; binding the same key twice warns.
4. **Built-in defaults.** An override on the same key shadows them; `maki.keymap.del` lifts the override so the default returns. Suspend is the only binding outside this layer, so every key is remappable except `Ctrl+Z`.

Only single-key bindings can be overridden. Multi-key combinations and non-key rows (like `Type` to filter) cannot.

The `/help` modal and the splash show default labels, not live overrides, but pressing the key still runs the override.

### Recovering from a bad keymap

If an override leaves Maki stuck (a rebound `Ctrl+C`, a modal that won't close, a plugin that throws on load), boot without user `init.lua`:

```bash
maki --no-plugins
```

Skips user `init.lua` files (global and project) but keeps the Lua host and builtin plugins running, so tools still work. `permissions.toml`, custom commands, and env files load as usual.

The default keymap lives in Rust, not Lua, so `--no-plugins` never drops it.

## Shell and images

These are input conventions, not remappable key rows:

- Prefix a line with `!` to run a shell command yourself (5 minute timeout). Use `!!` to hide the command and its output from the agent.
- `Ctrl+V` pastes an image from the clipboard into the prompt when the model supports vision. You can also paste image file paths.
