+++
title = "Keybindings"
weight = 7
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
| `Ctrl+N` / `Ctrl+P` | Next / previous task chat |
| `Ctrl+F` | Search messages |
| `Ctrl+S` | File picker |
| `Ctrl+O` | Open plan in editor |
| `Ctrl+T` | Toggle plan panel |
| `Ctrl+X` | Open tasks |

## Editing

| Key | Action |
|-----|--------|
| `Enter` | Submit prompt |
| `\+Enter` / `Ctrl+J` / `Alt+Enter` | Newline |
| `Tab` | Toggle mode |
| `/command` | Open command palette |
| `@` | Mention a file (Esc leaves a literal @) |
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
| Session Picker | `Ctrl+D` | Delete session |
| Queue | `Enter` | Remove item |
| Commands | `Tab` | Complete command |
| Model Picker | `!/@/#/$` | Set tier (strong/medium/weak/compaction) |

## Context Inheritance

Child contexts inherit their parent's bindings and add their own.

- **Pickers** is the base for: Task Picker, Session Picker, Rewind Picker, Theme Picker, Model Picker, Queue, Commands, Search, File Picker
