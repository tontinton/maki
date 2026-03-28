+++
title = "Keybindings"
weight = 7
+++

# Keybindings

On macOS, `Ctrl` maps to `Cmd` (⌘) where it makes sense.

## General

| Key | Action |
|-----|--------|
| Ctrl+C | Quit / clear input |
| Ctrl+H | Show keybindings |
| Ctrl+N | Next task chat |
| Ctrl+P | Previous task chat |
| Ctrl+F | Search messages |
| Ctrl+O | Open plan in editor |
| Ctrl+T | Toggle todo panel |
| Ctrl+X | Open tasks |

## Editing

| Key | Action |
|-----|--------|
| Enter | Submit prompt |
| Shift+Enter / Ctrl+J / Alt+Enter | Insert newline |
| Tab | Toggle mode |
| / | Open command palette (at start of input) |
| Ctrl+W | Delete word backward |
| Ctrl+K | Delete to end of line |
| Ctrl+A | Jump to start of line |
| Ctrl+U | Scroll half page up |
| Ctrl+D | Scroll half page down |
| Ctrl+Y | Scroll one line up |
| Ctrl+E | Scroll one line down |
| Ctrl+G | Scroll to top |
| Ctrl+B | Scroll to bottom |
| Ctrl+Q | Pop queue |
| Esc Esc | Rewind |

### macOS-specific

| Key | Action |
|-----|--------|
| ⌥⌫ | Delete word backward |
| ⌃← / ⌃→ | Move word left / right |
| ⌥Del | Delete word forward |
| ⌘← / ⌘→ | Jump to start / end of line |

## Streaming

While the agent is running:

| Key | Action |
|-----|--------|
| ↑ / ↓ | Navigate input history |
| Esc Esc | Cancel agent |

## Form Input

| Key | Action |
|-----|--------|
| ↑ / ↓ | Navigate options |
| Enter | Select option |
| Esc | Close |

## Picker

Pickers are used for sessions, models, themes, and more.

| Key | Action |
|-----|--------|
| ↑ / ↓ | Navigate |
| Enter | Select |
| Esc | Close |
| Type | Filter |

## Context-Specific

Some pickers add extra bindings on top of the defaults:

| Context | Key | Action |
|---------|-----|--------|
| Session Picker | Ctrl+D | Delete session |
| Queue Focus | Enter | Remove item |
| Command Palette | Tab | Toggle mode |

## Context Inheritance

Child contexts inherit their parent's bindings and add their own.

- **Picker** is the base for: TaskPicker, SessionPicker, RewindPicker, ThemePicker, ModelPicker, QueueFocus, CommandPalette, Search
- **SessionPicker** adds: Delete session
- **QueueFocus** adds: Remove item
- **CommandPalette** adds: Toggle mode
