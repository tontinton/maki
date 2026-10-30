+++
title = "Notifications"
weight = 9
[extra]
group = "Reference"
+++

# Notifications

Maki can tell you when a session finishes or needs your input. This is useful
when you move to another terminal while Maki works.

Notifications are enabled by default. A prompt that waits on you always
notifies, because the agent stays parked until you answer. `Agent turn
complete` is skipped only when Maki can tell you are watching: the terminal
reported focus, or you typed in the last 30 seconds.

Maki uses these messages:

- `Agent turn complete` or a preview of the response, up to 200 characters.
- `Permission requested: <tool>` for a permission prompt.
- `Authentication required` when authentication needs attention.
- `Question requested` for a question prompt.
- `Plan ready` when a plan is ready.

Response previews can appear in your operating system's notification history.
Maki does not include tool arguments, permission scopes, question bodies, plan
content, or error details. Use `bell` for a message-free alert, or use `off`
to disable notifications if response text should not reach notification
history.

## Configuration

Set `ui.notifications` in `~/.config/maki/init.lua`:

```lua
maki.setup({
  ui = {
    notifications = "auto",
  },
})
```

| Value | Behavior |
| --- | --- |
| `auto` | Use OSC 9 in a supported terminal. Use BEL otherwise. |
| `osc9` | Always send an OSC 9 notification. |
| `bell` | Always send the terminal bell. |
| `notify` | Show a desktop notification. |
| `off` | Do not send notifications. |

`auto` supports Ghostty, iTerm2, Kitty, Warp, and WezTerm. An unknown terminal
uses BEL. Your terminal settings decide whether BEL makes a sound or shows a
visual alert.

Maki also recognizes `xterm-ghostty` and `xterm-kitty` from `TERM`. This lets
OSC 9 work when an SSH connection does not preserve `TERM_PROGRAM`.

`notify` works on Linux, macOS, BSD, and Windows. On Linux it needs a D-Bus
session bus. The notification appears on the machine that runs Maki, so it
does not reach you over SSH.

## Program status

Maki also reports the state of your sessions with OSC 7501, the
[Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status).
A notification is a one-time alert. Program status always holds the current
state, so a supported terminal can show it next to the tab while you work
elsewhere.

| State | When |
| --- | --- |
| `idle` | No session is working or waiting. |
| `working` | A session is running a turn or a background task. |
| `blocked` | A session waits on you. `kind` is `permission`, `question`, or `auth`. A ready plan counts as `permission`. |
| `done` | A turn finished. The state stays until the next turn starts. |
| `error` | A turn stopped with an error. The state stays until the next turn starts. |

With several sessions open, Maki reports the most urgent one, in this order:
blocked, working, error, done, idle. A tie goes to the focused session. Each
report names the session by its title. A `blocked` report also says what Maki
waits for, such as `Permission requested: bash`. Maki cuts the title to 192
bytes and the message to 2048 bytes, the limits the protocol sets.

Maki clears the status when it exits or when you suspend it with `Ctrl+Z`,
and reports again after `fg`. With `--exit-on-done` the final `done` or
`error` stays, so the tab still shows how the run ended.

Program status is enabled by default. Terminals without OSC 7501 support
ignore it. To turn it off:

```lua
maki.setup({
  ui = {
    program_status = false,
  },
})
```

## tmux

OSC 9 and OSC 7501 need passthrough:

```tmux
set -g allow-passthrough all
```

Use `allow-passthrough all`, not `allow-passthrough on`. The `on` value permits
passthrough only while the Maki pane is visible. tmux drops the notification
or status report after you change to another tmux window.

In tmux and GNU screen, each pane or window reports its own status record.
Several Maki panes in one terminal do not overwrite each other, and quitting
one leaves the others in place.

Focus events are a separate setting:

```tmux
set -g focus-events on
```

This lets Maki suppress a turn completion you are already watching. Without it
Maki falls back to your last keypress and notifies for anything slower than 30
seconds.

Add the settings to `~/.tmux.conf`, then reload the file or restart tmux.

## Other terminal multiplexers

Maki wraps OSC 9 and OSC 7501 for GNU screen. GNU screen does not pass terminal focus
events to Maki, so Maki does not suppress notifications there. A notification
can appear while the GNU screen window has focus.

Maki sends OSC 9 and OSC 7501 directly through Zellij.

## Focus on Windows

This terminal focus protocol is not available on Windows. Maki treats the
terminal as unfocused so an explicit `bell` or `osc9` setting still works.
