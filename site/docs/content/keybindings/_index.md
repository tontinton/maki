+++
title = "Keybindings"
weight = 7
[extra]
group = "Reference"
+++

# Keybindings

On macOS, `Ctrl` maps to `Cmd` where it makes sense (run `/help` for exact keybindings).

## General

| Key | Action |
|-----|--------|
| `Ctrl+C` | Quit / clear input |
| `Ctrl+H` | Show keybindings |
| `Ctrl+N` / `Ctrl+P` | Next / previous task chat |
| `Ctrl+F` | Search messages |
| `Ctrl+S` | File picker |
| `Ctrl+O` | Open plan in editor |
| `Ctrl+T` | Toggle todo panel |
| `Ctrl+X` | Open tasks |

## Editing

| Key | Action |
|-----|--------|
| `Enter` | Submit prompt |
| `\+Enter` / `Ctrl+J` / `Alt+Enter` | Newline |
| `Tab` | Toggle mode |
| `/command` | Open command palette |
| `Ctrl+W` | Delete word backward |
| `Ctrl+U` / `Ctrl+D` | Scroll half page up / down |
| `Ctrl+Y` / `Ctrl+E` | Scroll one line up / down |
| `Ctrl+G` | Scroll to top |
| `Ctrl+B` | Scroll to bottom |
| `Ctrl+Q` | Pop queue |
| `Esc Esc` | Rewind |
| `Alt+O` | Edit input in external editor |

### macOS-specific

| Key | Action |
|-----|--------|
| `Ctrl+←` / `Ctrl+→` | Move word left / right |
| `Ctrl+Del` / `⌥Del` | Delete word forward |
| `Ctrl+K` | Delete to end of line |
| `Ctrl+A` | Jump to start of line |
| `⌘←` / `⌘→` | Jump to start/end of line |

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

## Context-Specific

Some pickers add extra bindings on top of the defaults:

| Context | Key | Action |
|---------|-----|--------|
| Session Picker | `Ctrl+D` | Delete session |
| Queue | `Enter` | Remove item |
| Commands | `Tab` | Toggle mode |
| Model Picker | `Ctrl+1/2/3` | Set tier (strong/medium/weak) |

## Context Inheritance

Child contexts inherit their parent's bindings and add their own.

- **Pickers** is the base for: Task Picker, Session Picker, Rewind Picker, Theme Picker, Model Picker, Queue, Commands, Search, File Picker
